//! `kiln install`: put the binary in place, write config, arrange start-on-boot, start now.
//!
//! * **Interactive** (default) may ask for a sudo password only when `--system` is requested; on
//!   Windows, Windows asks for administrator rights once, to start kiln at boot.
//! * **Unattended** (`--unattended`) never prompts: it installs per-user (no root needed), uses
//!   `sudo -n` for system installs (failing clearly if that would need a password), generates
//!   every secret itself and writes them `0600`.
//!
//! Boot integration only *launches* `kiln supervise`; restarting after failures is done by kiln's
//! own supervisor, so it works the same under systemd, cron, a desktop session, launchd, the
//! Windows Task Scheduler or a container.
//!
//! | OS      | `--boot auto`                                   | binary                                  |
//! |---------|-------------------------------------------------|-----------------------------------------|
//! | Linux   | systemd unit, else crontab `@reboot`, else XDG autostart | `~/.local/bin` (`/usr/local/bin` as root), put on PATH |
//! | macOS   | launchd: a LaunchAgent (a LaunchDaemon as root) | `~/.local/bin` (`/usr/local/bin` as root), put on PATH |
//! | Windows | a Scheduled Task at boot, as the user (administrator rights, asked for once), else one at sign-in (none), else the Startup folder | `%LOCALAPPDATA%\Programs\kiln`, put on the user's PATH |

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Context as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Boot {
    Auto,
    /// Linux: a systemd unit (user or system).
    Systemd,
    /// Linux, macOS: crontab `@reboot`.
    Cron,
    /// Linux desktops: XDG autostart (at login).
    Xdg,
    /// macOS: a LaunchAgent (per user) or LaunchDaemon (root).
    Launchd,
    /// Windows: a Scheduled Task at boot, running as you whether or not anyone signs in
    /// (administrator rights, asked for once); at your sign-in when they can't be had.
    Task,
    /// Windows: a Scheduled Task at your sign-in, in your session (no administrator rights).
    Logon,
    None,
}

/// Where `kiln install` puts a server (docs/p2p.md, "Installing").
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum On {
    /// The cluster of the kubeconfig's current context (or `--context`'s).
    Kubernetes,
    /// A container on this machine's Docker.
    Docker,
    /// This machine itself, as a service.
    Machine,
}

#[derive(Debug, Clone, clap::Args)]
pub struct InstallArgs {
    /// Never prompt (no passwords, no questions). Fails clearly instead.
    #[arg(long)]
    pub unattended: bool,
    /// System-wide install (root / sudo): /usr/local/bin, /etc/kiln, /var/lib/kiln.
    #[arg(long)]
    pub system: bool,
    /// Install an agent instead of a server.
    #[arg(long)]
    pub agent: bool,
    #[arg(long, value_enum, default_value = "auto")]
    pub boot: Boot,
    /// Do not start it now.
    #[arg(long)]
    pub no_start: bool,
    /// Agent: server URL.
    #[arg(long, env = "KILN_SERVER")]
    pub server: Option<String>,
    /// Agent: QUIC address to dial (overrides what the server advertises, e.g. when the server
    /// runs in a cluster and is reached through forwarded ports).
    #[arg(long, env = "KILN_QUIC")]
    pub quic: Option<String>,
    /// Agent: join token (consumed during install; never stored in the boot command).
    #[arg(long, env = "KILN_JOIN_TOKEN", allow_hyphen_values = true)]
    pub token: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long, value_delimiter = ',')]
    pub labels: Vec<String>,
    #[arg(long)]
    pub slots: Option<u32>,
    /// Agent: start this version beside the installed agent instead of restarting it. The old
    /// runner keeps its terminals, agent sessions and jobs until they end, then retires itself;
    /// everything new goes to this one. Needs no token or password (the machine proves itself
    /// with its own certificate). Safe to re-run.
    #[arg(long, requires = "agent")]
    pub upgrade: bool,
    /// Agent: the installed agent's service (unit, launchd label or task name) for --upgrade.
    #[arg(long, default_value = "kiln-agent")]
    pub service: String,
    /// Where a server goes: Kubernetes (the default; the kubeconfig's current context), Docker on
    /// this machine, or this machine itself (the default with --system). An agent goes on this
    /// machine.
    #[arg(long, value_enum)]
    pub on: Option<On>,
    /// Kubernetes: the kubeconfig context to install into, rather than the current one.
    #[arg(long)]
    pub context: Option<String>,
    /// Kubernetes: the namespace to install into.
    #[arg(long, default_value = "kiln")]
    pub namespace: String,
    /// Kubernetes, Docker: the image to run, rather than one built from this binary.
    #[arg(long)]
    pub image: Option<String>,
    /// Kubernetes: a registry to push the image built from this binary to, for a cluster that
    /// can't take it from this machine's Docker (minikube's, kind's, k3d's and Docker Desktop's can).
    #[arg(long)]
    pub registry: Option<String>,
    /// Docker: the port on this machine for kiln's UI and API (QUIC takes the next one).
    #[arg(long, default_value_t = 7070)]
    pub port: u16,
}

impl InstallArgs {
    /// Where this install goes: an agent on this machine; a server where `--on` says, else on
    /// this machine for a system-wide install (`--system`), else in Kubernetes.
    pub fn target(&self) -> anyhow::Result<On> {
        match (self.agent, self.on) {
            (true, None | Some(On::Machine)) => Ok(On::Machine),
            (true, Some(_)) => anyhow::bail!("an agent is installed on this machine: leave out --on"),
            (false, Some(on)) if on != On::Machine && self.system => anyhow::bail!("--system installs on this machine: give --on machine, or leave out --system"),
            (false, Some(on)) => Ok(on),
            (false, None) if self.system => Ok(On::Machine),
            (false, None) => Ok(On::Kubernetes),
        }
    }
}

fn is_root() -> bool {
    crate::os::is_root()
}

fn home() -> PathBuf {
    crate::os::home_dir()
}

fn have(bin: &str) -> bool {
    crate::os::have(bin)
}

/// Run quietly; whether it succeeded.
fn quiet(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Where the binary is installed.
pub(crate) fn bin_path(system: bool) -> PathBuf {
    if system {
        PathBuf::from("/usr/local/bin/kiln")
    } else if cfg!(windows) {
        crate::os::data_home().join("Programs").join("kiln").join("kiln.exe")
    } else {
        home().join(".local/bin/kiln")
    }
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let st = cmd.status().with_context(|| format!("run {cmd:?}"))?;
    if !st.success() {
        anyhow::bail!("{cmd:?} exited with {st}");
    }
    Ok(())
}

pub(crate) fn say(msg: impl AsRef<str>) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "  {}", msg.as_ref());
}

/// Copy the running executable to `dest` atomically (no-op if it already is `dest`).
pub(crate) fn place_binary(dest: &Path) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    if exe.canonicalize().ok() == dest.canonicalize().ok() {
        return Ok(());
    }
    let dir = dest.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    // Old binaries moved aside by an earlier upgrade (Windows), once nothing runs them any more.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("kiln") && name.contains(".old-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let tmp = dest.with_extension(format!("new.{}", std::process::id()));
    std::fs::copy(&exe, &tmp).with_context(|| format!("copy to {}", tmp.display()))?;
    crate::os::make_executable(&tmp)?;
    sign_placed(&tmp, dest);
    if let Err(e) = std::fs::rename(&tmp, dest) {
        // Windows cannot replace a running executable, but it can rename it out of the way.
        let aside = dest.with_extension(format!("old-{}", std::process::id()));
        if !dest.exists() || std::fs::rename(dest, &aside).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("replace {}", dest.display()));
        }
        std::fs::rename(&tmp, dest).with_context(|| format!("replace {}", dest.display()))?;
    }
    Ok(())
}

/// The binary's folder on the user's PATH, so `kiln` works in every new terminal: on Windows in
/// the user's own PATH; elsewhere, when this terminal's PATH hasn't it (`~/.local/bin` on a Mac),
/// through a line in the startup file of the user's shell.
fn put_on_path(bin: &Path) {
    let Some(dir) = bin.parent() else { return };
    match crate::os::add_to_user_path(dir) {
        Ok(Some(added)) => {
            let place = added.file.map(|f| format!(" in {}", f.display())).unwrap_or_default();
            say(format!("path        {} added to your PATH{place} (new terminals find `kiln`)", dir.display()));
            if let Some(now) = added.now {
                say(format!("            for this terminal too, run: {now}"));
            }
        }
        Ok(None) => say(format!("path        {} is on your PATH", dir.display())),
        Err(e) => say(format!("path        {} not added to your PATH ({e}): run kiln as {}", dir.display(), bin.display())),
    }
}

pub async fn install(a: InstallArgs) -> anyhow::Result<()> {
    match a.target()? {
        On::Kubernetes => {
            eprintln_header("Installing kiln on Kubernetes");
            return crate::install_targets::kubernetes_install(&a).await;
        }
        On::Docker => {
            eprintln_header("Installing kiln in Docker");
            return crate::install_targets::docker_install(&a).await;
        }
        On::Machine => {}
    }
    if a.system && cfg!(windows) {
        anyhow::bail!("--system is not supported on Windows: kiln installs per user and needs no administrator rights");
    }
    if a.system && !is_root() {
        // Re-run ourselves under sudo. Unattended → `sudo -n` (never prompts).
        let exe = std::env::current_exe()?;
        let mut cmd = Command::new("sudo");
        if a.unattended {
            cmd.arg("-n");
        }
        cmd.arg("--preserve-env=KILN_JOIN_TOKEN,KILN_SERVER").arg(exe).args(std::env::args().skip(1));
        let st = cmd.status().context("run sudo")?;
        if !st.success() {
            anyhow::bail!(
                "system install needs root. {}",
                if a.unattended {
                    "Passwordless sudo is not available; re-run without --system for a per-user install, or run once as root."
                } else {
                    "sudo failed."
                }
            );
        }
        return Ok(());
    }
    if a.agent && a.upgrade {
        eprintln_header("Upgrading the kiln agent beside the running one");
        return crate::upgrade::install_alongside(&a.service, &|m| say(m)).await;
    }
    let system = a.system || is_root();
    let bin = bin_path(system);
    let config_path = crate::config::default_config_path();
    // A server installed where none was: a failure takes back what it made, and nothing else may
    // answer on its port (the check would ask that instead).
    let fresh = !a.agent && !config_path.exists();
    let (had_bin, data_dir) = (bin.exists(), crate::config::default_data_dir());
    let had_data = data_dir.exists();
    let ours = ["server".to_owned()];
    let running_before = if fresh { supervisors_of(&ours) } else { Vec::new() };
    if fresh {
        let addr = crate::config::Config::load(&config_path)?.server.http_addr;
        if let Err(e) = std::net::TcpListener::bind(&addr) {
            anyhow::bail!("kiln's address {addr} is taken here ({e}): stop what listens on it, or set KILN_HTTP_ADDR");
        }
    }
    eprintln_header(if a.agent { "Installing kiln agent" } else { "Installing kiln server" });
    match install_here(&a, system, &bin, &config_path).await {
        Err(e) if fresh => {
            say("");
            say("taking back the install, which didn't come up");
            let started: Vec<u32> = supervisors_of(&ours).into_iter().filter(|p| !running_before.contains(p)).collect();
            take_back(&started, &config_path, (!had_bin).then_some(bin.as_path()), (!had_data).then_some(data_dir.as_path()));
            Err(e.context("kiln didn't come up on this machine; nothing was left: what the install made was removed"))
        }
        done => done,
    }
}

/// Installs kiln on this machine: its binary at `bin`, its config at `config_path`, started at
/// boot and now. A server's install ends once it answers, its UI loads and its runner is connected.
async fn install_here(a: &InstallArgs, system: bool, bin: &Path, config_path: &Path) -> anyhow::Result<()> {
    place_binary(bin)?;
    say(format!("binary      {}", bin.display()));
    put_on_path(bin);

    let mut state_dir = crate::config::default_data_dir().join("agent");
    let (unit, args): (&str, Vec<String>) = if a.agent {
        ("kiln-agent", agent_setup(a).await?)
    } else {
        if !config_path.exists() {
            let cfg = crate::config::Config::default();
            let text = format!(
                "# kiln configuration. Every key is optional; see docs/configuration.md.\n\n{}",
                toml::to_string_pretty(&cfg).unwrap_or_default()
            );
            crate::util::write_secret(config_path, text.as_bytes(), 0o640)?;
            say(format!("config      {} (new)", config_path.display()));
        } else {
            say(format!("config      {} (kept)", config_path.display()));
        }
        let cfg = crate::config::Config::load(config_path)?;
        let token = crate::server::admin_token(&cfg)?;
        say(format!("admin token {}", crate::server::token_path(&cfg).display()));
        let _ = token;
        state_dir = cfg.server.data_dir.clone();
        ("kiln", vec!["server".to_owned()])
    };

    let mut sup_args = vec!["supervise".to_owned(), "--".to_owned()];
    sup_args.extend(args);
    let ask = !a.unattended && crate::join::interactive();
    let boot = Install { system, unit, bin, args: &sup_args, config: config_path, state_dir: &state_dir, env: None, ask };
    let started_by_boot = setup_boot(a.boot, &boot)?;

    let mut running = started_by_boot;
    if !a.no_start && !started_by_boot {
        if cfg!(windows) && crate::os::elevated() {
            // Started from here, kiln (and everything it runs) would have this shell's
            // administrator rights; its Scheduled Task never gives it those.
            say("started     no: not from an elevated shell; it starts at the next sign-in or boot");
        } else {
            running = start_replacing(&boot, || {
                let mut cmd = Command::new(bin);
                cmd.arg("supervise").arg("--detach").args(sup_args.iter().skip(1));
                run(&mut cmd)?;
                say("started     kiln supervise (detached)");
                Ok(true)
            })?;
        }
    }

    if !a.agent {
        let cfg = crate::config::Config::load(config_path)?;
        let token = crate::server::admin_token(&cfg)?;
        let url = local_url(&cfg);
        if running {
            let runners = crate::install_targets::verify_url(&url, crate::install_targets::READY_TIME).await?;
            crate::install_targets::opened(&url, &token, runners, "kiln login");
        } else {
            say("");
            say(format!("UI          {url}"));
            say(format!("login link  {} (valid 5 min; `kiln login` makes a new one)", crate::api::otp_url(&token, &url)));
        }
    }
    Ok(())
}

/// Takes back a server's install on this machine that didn't come up: its service at boot, the
/// supervisors it `started`, its config, and its binary and data when they were new (`bin`, `data`).
fn take_back(started: &[u32], config_path: &Path, bin: Option<&Path>, data: Option<&Path>) {
    remove_service(false);
    if !started.is_empty() {
        crate::os::supervise::stop(started);
        say(format!("stopped     kiln supervise (pid {})", started.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")));
    }
    for (path, dir) in [(Some(config_path), false), (data, true), (bin, false)] {
        let Some(path) = path.filter(|p| p.exists()) else { continue };
        match if dir { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) } {
            Ok(()) => say(format!("removed     {}", path.display())),
            Err(e) => say(format!("not removed {} ({e})", path.display())),
        }
    }
}

fn local_url(cfg: &crate::config::Config) -> String {
    let port = cfg.server.http_addr.rsplit_once(':').map_or("7070", |(_, p)| p);
    format!("http://localhost:{port}")
}

fn eprintln_header(s: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "\n{s}\n");
}

/// Enroll now (so the boot command carries no secret) and return the agent's run arguments.
async fn agent_setup(a: &InstallArgs) -> anyhow::Result<Vec<String>> {
    let server = a.server.clone().ok_or_else(|| anyhow::anyhow!("--server is required for an agent"))?;
    let name = a.name.clone().unwrap_or_else(crate::config::Config::hostname);
    let labels = if a.labels.is_empty() { vec!["baremetal".to_owned()] } else { a.labels.clone() };
    let state_dir = crate::config::default_data_dir().join("agent");
    let opts = crate::agent::Opts {
        server_url: server.clone(),
        quic: a.quic.clone(),
        name: name.clone(),
        labels: labels.clone(),
        slots: a.slots.unwrap_or(1),
        work_dir: state_dir.join("work"),
        state_dir: state_dir.clone(),
        join_token: a.token.clone(),
        ephemeral: false,
        dedicated_job: None,
        terminal: true,
        job_terminal: true,
        shell: None,
        credentials: None,
        instance: None,
        config: None,
        reach: Some(true),
        outpost: false,
        declared: crate::runner::probe::Declared::default(),
    };
    crate::agent::enroll_only(&opts).await?;
    say(format!("enrolled    as {} with {server}", crate::pki::sanitize_name(&name)));
    let mut args = vec![
        "agent".to_owned(),
        "--server".to_owned(),
        server,
        "--name".to_owned(),
        name,
        "--labels".to_owned(),
        labels.join(","),
    ];
    if let Some(q) = &a.quic {
        args.push("--quic".to_owned());
        args.push(q.clone());
    }
    if let Some(s) = a.slots {
        args.push("--slots".to_owned());
        args.push(s.to_string());
    }
    Ok(args)
}

/// Install and start one more service — a runner beside an existing one — with the platform's
/// usual boot integration for `unit`, leaving every other service alone.
pub fn start_service(system: bool, unit: &str, bin: &Path, args: &[String], state_dir: &Path, env: &[String]) -> anyhow::Result<()> {
    let config = crate::config::default_config_path();
    let i = Install { system, unit, bin, args, config: &config, state_dir, env: (!env.is_empty()).then_some(env), ask: false };
    if !setup_boot(Boot::Auto, &i)? {
        let mut cmd = Command::new(bin);
        cmd.arg("supervise").arg("--detach").args(args.iter().skip(1));
        run(&mut cmd)?;
    }
    Ok(())
}

/// What is being installed, for the boot integrations.
struct Install<'a> {
    /// System-wide (root): a systemd system unit or a LaunchDaemon.
    system: bool,
    /// `kiln` (server) or `kiln-agent`: the unit / task name.
    unit: &'a str,
    bin: &'a Path,
    /// `supervise -- <subcommand> …`
    args: &'a [String],
    config: &'a Path,
    /// The supervisor's state directory (its `kiln.log` lives there).
    state_dir: &'a Path,
    /// The service's environment (`KEY=value`), when it must match another service's exactly
    /// (a runner set up beside an existing one); else kiln's config path and this shell's PATH.
    env: Option<&'a [String]>,
    /// A person is here to ask (not `--unattended`, at a terminal): on Windows, for the
    /// administrator rights a Scheduled Task at boot takes.
    ask: bool,
}

impl Install<'_> {
    /// `supervise --detach -- <subcommand> …`: what launchers that do not supervise run.
    fn detach_args(&self) -> Vec<String> {
        let mut v = vec!["supervise".to_owned(), "--detach".to_owned()];
        v.extend(self.args.iter().skip(1).cloned());
        v
    }

    /// [`Self::detach_args`] as one Windows command line.
    fn detach_command_line(&self) -> String {
        self.detach_args().iter().map(|a| win_quote(a)).collect::<Vec<_>>().join(" ")
    }
}

/// Returns true when the boot mechanism also started the service now.
fn setup_boot(boot: Boot, i: &Install<'_>) -> anyhow::Result<bool> {
    let systemd = have("systemctl") && Path::new("/run/systemd/system").exists();
    let boot = match boot {
        Boot::Auto if cfg!(windows) => Boot::Task,
        Boot::Auto if cfg!(target_os = "macos") => Boot::Launchd,
        Boot::Auto if systemd => Boot::Systemd,
        Boot::Auto if have("crontab") => Boot::Cron,
        Boot::Auto if !i.system => Boot::Xdg,
        Boot::Auto => Boot::None,
        b => b,
    };
    let (system, unit, bin, args, config) = (i.system, i.unit, i.bin, i.args, i.config);
    let exec = format!(
        "{} {}",
        bin.display(),
        args.iter().map(|s| shell_quote(s)).collect::<Vec<_>>().join(" ")
    );
    match boot {
        Boot::Systemd => {
            let env = match i.env {
                Some(list) => list.iter().map(|kv| format!("Environment=\"{}\"\n", kv.replace('"', "").replace('%', "%%"))).collect::<String>(),
                None => format!(
                    "Environment=KILN_CONFIG={cfg}\n\
                     # The installing user's PATH, so jobs find the same toolchains (nvm, cargo, uv, go).\n\
                     Environment=\"PATH={path}\"\n",
                    cfg = config.display(),
                    path = std::env::var("PATH").unwrap_or_default().replace('%', "%%").replace('"', ""),
                ),
            };
            let body = format!(
                "[Unit]\nDescription=kiln {unit} (self-supervising CI/CD job server)\nAfter=network-online.target\nWants=network-online.target\n\n\
                 [Service]\nType=simple\n{env}ExecStart={exec}\n\
                 # kiln supervises and restarts its own server; this only covers the supervisor itself.\n\
                 Restart=always\nRestartSec=5\nKillMode=mixed\nTimeoutStopSec=30\nLimitNOFILE=1048576\n\n\
                 [Install]\nWantedBy={target}\n",
                target = if system { "multi-user.target" } else { "default.target" },
            );
            let (path, user_flag): (PathBuf, &[&str]) = if system {
                (PathBuf::from(format!("/etc/systemd/system/{unit}.service")), &[])
            } else {
                (home().join(format!(".config/systemd/user/{unit}.service")), &["--user"])
            };
            crate::util::write_secret(&path, body.as_bytes(), 0o644)?;
            run(Command::new("systemctl").args(user_flag).arg("daemon-reload"))?;
            run(Command::new("systemctl").args(user_flag).args(["enable", unit]))?;
            // restart (not just start) so a re-install applies the new unit and binary.
            run(Command::new("systemctl").args(user_flag).args(["restart", unit]))?;
            say(format!("boot        systemd {} unit {}", if system { "system" } else { "user" }, path.display()));
            if !system {
                // Start at boot without a login session. Allowed for one's own user by default
                // polkit policy; never prompts (--no-ask-password).
                let user = std::env::var("USER").unwrap_or_default();
                let ok = Command::new("loginctl")
                    .args(["--no-ask-password", "enable-linger", &user])
                    .status()
                    .is_ok_and(|s| s.success());
                if ok {
                    say("linger      enabled (starts at boot, before login)");
                } else {
                    say(format!("linger      not enabled: starts at login. For boot without login run once: sudo loginctl enable-linger {user}"));
                }
            }
            Ok(true)
        }
        Boot::Cron => {
            let line = format!("@reboot {} supervise --detach {}", bin.display(), args.iter().skip(1).map(|s| shell_quote(s)).collect::<Vec<_>>().join(" "));
            let current = Command::new("crontab").arg("-l").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
            if !current.lines().any(|l| l.trim() == line) {
                let mut child = Command::new("crontab").arg("-").stdin(std::process::Stdio::piped()).spawn()?;
                if let Some(mut stdin) = child.stdin.take() {
                    use std::io::Write as _;
                    let text: String = current.lines().filter(|l| !l.contains(&format!("{} supervise", bin.display()))).map(|l| format!("{l}\n")).collect();
                    stdin.write_all(format!("{text}{line}\n").as_bytes())?;
                }
                let _ = child.wait()?;
            }
            say("boot        crontab @reboot");
            Ok(false)
        }
        Boot::Xdg => {
            let path = home().join(format!(".config/autostart/{unit}.desktop"));
            let body = format!(
                "[Desktop Entry]\nType=Application\nName=kiln {unit}\nExec={} supervise --detach {}\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
                bin.display(),
                args.iter().skip(1).map(|s| shell_quote(s)).collect::<Vec<_>>().join(" ")
            );
            crate::util::write_secret(&path, body.as_bytes(), 0o644)?;
            say(format!("boot        XDG autostart {} (starts at login)", path.display()));
            Ok(false)
        }
        Boot::Launchd => launchd(i),
        Boot::Task => scheduled_task(i, TaskStart::Boot),
        Boot::Logon => scheduled_task(i, TaskStart::SignIn),
        Boot::None | Boot::Auto => {
            say("boot        none (start manually: kiln supervise -- server)");
            Ok(false)
        }
    }
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=,@".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

// ----------------------------------------------------------------- an earlier install's runner

/// Start the service (`start`), stopping what an earlier install of it left running with other
/// arguments (another name, labels or server): a supervisor only keeps out those started with its
/// own arguments, so it would go on beside the new one, two runners on one state directory.
/// Stopped first; after, when this process runs under one of them (from that runner's terminal
/// or agent), as stopping it ends this process too.
fn start_replacing(i: &Install<'_>, start: impl FnOnce() -> anyhow::Result<bool>) -> anyhow::Result<bool> {
    let ours: Vec<String> = i.args.iter().skip_while(|a| *a != "--").skip(1).cloned().collect();
    let (stale, inside) = stale_supervisors(&ours);
    if stale.is_empty() {
        return start();
    }
    let pids = stale.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
    say(format!("stopping    kiln an earlier install started with other arguments (pid {pids})"));
    if !inside {
        crate::os::supervise::stop(&stale);
        return start();
    }
    let started = start()?;
    say("            this terminal runs under it, and ends with it");
    crate::os::supervise::stop(&stale);
    Ok(started)
}

/// This user's supervisors of the service `ours` (what follows `supervise --`) that run other
/// arguments ([`stale_in`]), and whether this process runs under one of them.
fn stale_supervisors(ours: &[String]) -> (Vec<u32>, bool) {
    use sysinfo::Process;
    let (sys, procs) = user_procs();
    let me = sysinfo::get_current_pid().ok();
    let stale = stale_in(&procs, ours, &crate::config::default_data_dir().join("agent"));
    let mut up = me.and_then(|p| sys.process(p)).and_then(Process::parent);
    let mut inside = false;
    for _ in 0..64 {
        let Some(p) = up else { break };
        if stale.contains(&p.as_u32()) {
            inside = true;
            break;
        }
        up = sys.process(p).and_then(Process::parent);
    }
    (stale, inside)
}

/// This user's processes, but this one: the system's view, and each one's pid and command line.
fn user_procs() -> (sysinfo::System, Vec<(u32, Vec<String>)>) {
    use sysinfo::{Process, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always).with_user(UpdateKind::Always));
    let me = sysinfo::get_current_pid().ok();
    let user = me.and_then(|p| sys.process(p)).and_then(Process::user_id).cloned();
    let procs = sys
        .processes()
        .iter()
        .filter(|(pid, p)| Some(**pid) != me && user.as_ref().is_none_or(|u| p.user_id() == Some(u)))
        .map(|(pid, p)| (pid.as_u32(), p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect()))
        .collect();
    (sys, procs)
}

/// This user's supervisors running the service `ours` (what follows `supervise --`) as it is.
fn supervisors_of(ours: &[String]) -> Vec<u32> {
    running_in(&user_procs().1, ours)
}

/// Of `procs` (`(pid, command line)`), the supervisors running exactly `ours` after `supervise --`.
fn running_in(procs: &[(u32, Vec<String>)], ours: &[String]) -> Vec<u32> {
    procs
        .iter()
        .filter_map(|(pid, argv)| {
            let sep = argv.iter().position(|w| w == "--")?;
            (argv.get(1..sep)?.iter().any(|w| w == "supervise") && argv.get(sep + 1..)? == ours).then_some(*pid)
        })
        .collect()
}

/// Of `procs` (`(pid, command line)`), the supervisors of the service `ours` (what follows
/// `supervise --`) that run other arguments: `<kiln> supervise [--exe <kiln>] -- <its subcommand> …`
/// on the same state directory (`--state-dir`, else `default_state`). Never a runner set up beside
/// it (`--instance`: `kiln install --agent --upgrade`), which is meant to run beside it.
fn stale_in(procs: &[(u32, Vec<String>)], ours: &[String], default_state: &Path) -> Vec<u32> {
    let state = |args: &[String]| {
        let dir = crate::upgrade::arg_value(args, "--state-dir").map_or_else(|| default_state.to_path_buf(), PathBuf::from);
        dir.canonicalize().unwrap_or(dir)
    };
    let our_state = state(ours);
    procs
        .iter()
        .filter_map(|(pid, argv)| {
            let sep = argv.iter().position(|w| w == "--")?;
            let (before, after) = (argv.get(1..sep)?, argv.get(sep + 1..)?);
            let same_service = before.iter().any(|w| w == "supervise") && after.first() == ours.first() && !after.iter().any(|w| w == "--instance") && state(after) == our_state;
            (same_service && after != ours).then_some(*pid)
        })
        .collect()
}

// ---------------------------------------------------------------------------------- launchd

/// The launchd label of a unit (`kiln` → `io.github.hyper-light.kiln`).
pub(crate) fn launchd_label(unit: &str) -> String {
    format!("io.github.hyper-light.{unit}")
}

/// Where a unit's plist lives: a LaunchDaemon as root, else the user's LaunchAgent.
pub(crate) fn launchd_plist(system: bool, unit: &str) -> PathBuf {
    let file = format!("{}.plist", launchd_label(unit));
    if system { PathBuf::from("/Library/LaunchDaemons").join(file) } else { home().join("Library/LaunchAgents").join(file) }
}

/// The launchd domains a unit may be loaded in: the system domain for a daemon; for an agent the
/// GUI session (`gui/<uid>`), or the background session an SSH login has (`user/<uid>`).
fn launchd_domains(system: bool) -> Vec<String> {
    if system {
        vec!["system".to_owned()]
    } else {
        let uid = crate::os::uid();
        vec![format!("gui/{uid}"), format!("user/{uid}")]
    }
}

/// The program a unit's launchd job runs: a copy of kiln that upgrades leave alone
/// (`supervise --exe <kiln> -- …`), so the job itself stays what macOS was told about. That alone
/// doesn't keep kiln on the local network across upgrades: macOS judges the new build it starts by
/// its own signature, and an ad hoc signed build is a new program to it (on 2026-10-18 a fresh
/// build an unchanged launcher started was refused all the same). What does is kiln's signing
/// identity on the Mac (`crate::os::macsign`, `kiln signing setup`), which every binary installed
/// here is signed with.
pub(crate) fn launcher_path(unit: &str) -> PathBuf {
    crate::config::default_data_dir().join("launcher").join(unit)
}

/// The unit's launcher, made from `bin` if it has none that runs yet. One that runs is kept: a
/// new one would be a new program to macOS, to be allowed onto the local network again.
fn ensure_launcher(unit: &str, bin: &Path) -> anyhow::Result<PathBuf> {
    let launcher = launcher_path(unit);
    let runs = Command::new(&launcher)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && o.stdout.starts_with(b"kiln"));
    if runs {
        return Ok(launcher);
    }
    let dir = launcher.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = launcher.with_extension(format!("new.{}", std::process::id()));
    std::fs::copy(bin, &tmp).with_context(|| format!("copy {} to {}", bin.display(), tmp.display()))?;
    crate::os::make_executable(&tmp)?;
    sign_placed(&tmp, &launcher);
    std::fs::rename(&tmp, &launcher).with_context(|| format!("install {}", launcher.display()))?;
    Ok(launcher)
}

/// On a Mac with kiln's signing identity (`kiln signing setup`), the copy about to become `dest`
/// signed with it, so macOS knows it as the program it let onto the local network before.
fn sign_placed(tmp: &Path, dest: &Path) {
    if let Err(e) = crate::os::macsign::sign_as(tmp, dest, &crate::updates::own_build()) {
        say(format!("couldn't sign {} as kiln ({e:#}): macOS may ask again before it reaches the server", dest.display()));
    }
}

/// A launchd job running `kiln supervise -- …` at load (login, or boot for a daemon), from the
/// unit's launcher when it has one (`<launcher> supervise --exe <bin> -- …`). launchd restarts
/// the supervisor only if it fails (a clean exit means it was stopped, or another supervisor
/// already runs the service); the supervisor restarts kiln itself.
fn launchd_body(i: &Install<'_>, log: &Path, launcher: Option<&Path>) -> String {
    let mut program = match launcher {
        Some(l) => vec![l.display().to_string(), "supervise".to_owned(), "--exe".to_owned(), i.bin.display().to_string()],
        None => vec![i.bin.display().to_string(), "supervise".to_owned()],
    };
    // `i.args` is `supervise -- …`.
    program.extend(i.args.iter().skip(1).cloned());
    let program: String = program.iter().map(|a| format!("\n    <string>{}</string>", xml_escape(a))).collect();
    let env: Vec<(String, String)> = match i.env {
        Some(list) => list.iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
        None => vec![
            ("KILN_CONFIG".to_owned(), i.config.display().to_string()),
            ("PATH".to_owned(), std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_owned())),
        ],
    };
    let env: String = env.iter().map(|(k, v)| format!("\n    <key>{}</key>\n    <string>{}</string>", xml_escape(k), xml_escape(v))).collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>{program}
  </array>
  <key>EnvironmentVariables</key>
  <dict>{env}
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>SoftResourceLimits</key>
  <dict>
    <key>NumberOfFiles</key>
    <integer>10240</integer>
  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(&launchd_label(i.unit)),
        log = xml_escape(&log.display().to_string()),
    )
}

/// Run `launchctl` quietly: `Ok` if it succeeded, else what it said.
pub(crate) fn launchctl(args: &[&str]) -> Result<(), String> {
    match Command::new("launchctl").args(args).stdin(Stdio::null()).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let said = String::from_utf8_lossy(&o.stderr).trim().to_owned();
            Err(if said.is_empty() { format!("launchctl {} exited with {}", args.join(" "), o.status) } else { said })
        }
        Err(e) => Err(format!("run launchctl: {e}")),
    }
}

/// Whether `domain/label` is loaded, and its process if one runs (`Some(None)`: loaded, not running).
fn launchd_job(domain: &str, label: &str) -> Option<Option<u32>> {
    let out = Command::new("launchctl").args(["print", &format!("{domain}/{label}")]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.lines().find_map(|l| l.trim().strip_prefix("pid = ")).and_then(|p| p.trim().parse().ok()))
}

/// The parent of process `pid` (`ps`), for walking up from this process.
fn parent_of(pid: u32) -> Option<u32> {
    let out = Command::new("ps").args(["-o", "ppid=", "-p", &pid.to_string()]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Whether process `pid` is this process or one of its ancestors.
fn is_ancestor_of_me(pid: u32) -> bool {
    let mut p = std::process::id();
    for _ in 0..64 {
        if p == pid {
            return true;
        }
        match parent_of(p) {
            Some(next) if next > 1 && next != p => p = next,
            _ => return false,
        }
    }
    false
}

/// Load `plist` (label `label`) afresh in `domain` from a one-shot launchd job of its own, which
/// outlives what it stops: it stops `stop_first` (a supervisor launchd didn't start) if given,
/// unloads the job, waits until launchd let go of it, loads it again, then unloads itself. Used
/// when the caller runs inside the job it reloads, so it can't wait for that itself.
fn reload_later(domain: &str, label: &str, plist: &Path, stop_first: Option<u32>, dir: &Path) -> anyhow::Result<()> {
    const SCRIPT: &str = r#"sleep 2
if [ -n "$SUP" ]; then
  kill -TERM "$SUP" 2>/dev/null
  i=0; while kill -0 "$SUP" 2>/dev/null && [ $i -lt 60 ]; do sleep 1; i=$((i+1)); done
  kill -KILL "$SUP" 2>/dev/null
fi
launchctl bootout "$D/$L" 2>/dev/null
i=0; while launchctl print "$D/$L" >/dev/null 2>&1 && [ $i -lt 60 ]; do sleep 1; i=$((i+1)); done
i=0; until launchctl bootstrap "$D" "$P"; do i=$((i+1)); if [ $i -ge 60 ]; then echo "kiln: launchd would not load $P"; break; fi; sleep 1; done
launchctl bootout "$D/$L.reload""#;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let me = format!("{label}.reload");
    let path = dir.join(format!("{me}.plist"));
    let log = dir.join("reload.log");
    let mut env = vec![("D", domain.to_owned()), ("L", label.to_owned()), ("P", plist.display().to_string())];
    if let Some(pid) = stop_first {
        env.push(("SUP", pid.to_string()));
    }
    let env: String = env.iter().map(|(k, v)| format!("\n    <key>{k}</key>\n    <string>{}</string>", xml_escape(v))).collect();
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{me}</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>-c</string>
    <string>{script}</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>{env}
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        me = xml_escape(&me),
        script = xml_escape(SCRIPT),
        log = xml_escape(&log.display().to_string()),
    );
    crate::util::write_secret(&path, body.as_bytes(), 0o644)?;
    // One left from before (it unloads itself when done; one that failed may not have).
    let _ = launchctl(&["bootout", &format!("{domain}/{me}")]);
    let p = path.display().to_string();
    launchctl(&["bootstrap", domain, &p]).map_err(|e| anyhow::anyhow!("load {}: {e}", path.display()))
}

/// Have launchd start this machine's runner afresh (macOS): its job rewritten to run from the
/// unit's launcher if it doesn't yet, then reloaded from a one-shot job ([`reload_later`]),
/// which also stops a runner launchd didn't start (one started by hand, or by an install whose
/// load failed). For a runner that can't take a new binary up as a fresh process from its own
/// supervisor. `false` when there is no launchd job for it. Blocking.
pub fn relaunch_runner(state_dir: &Path, instance: Option<&str>) -> anyhow::Result<bool> {
    if !cfg!(target_os = "macos") {
        return Ok(false);
    }
    let unit = crate::updates::runner_service(state_dir, instance);
    let Some(existing) = crate::upgrade::find_existing(&unit) else { return Ok(false) };
    let label = launchd_label(&unit);
    let plist = launchd_plist(existing.system, &unit);
    if existing.launcher.is_none() {
        let launcher = ensure_launcher(&unit, &existing.bin)?;
        let mut args = vec!["supervise".to_owned(), "--".to_owned()];
        args.extend(existing.args.iter().cloned());
        let config = crate::config::default_config_path();
        let env = (!existing.env.is_empty()).then_some(existing.env.as_slice());
        let i = Install { system: existing.system, unit: &unit, bin: &existing.bin, args: &args, config: &config, state_dir, env, ask: false };
        crate::util::write_secret(&plist, launchd_body(&i, &state_dir.join("kiln.log"), Some(&launcher)).as_bytes(), 0o644)?;
        tracing::info!(plist = %plist.display(), launcher = %launcher.display(), "the runner's launchd job runs from its launcher now");
    }
    let domains = launchd_domains(existing.system);
    let loaded = domains.iter().find_map(|d| launchd_job(d, &label).map(|pid| (d.clone(), pid)));
    // What to stop first when launchd isn't running this runner: its supervisor, or this process
    // when nothing supervises it.
    let parent = crate::os::parent_pid();
    let ours = if parent > 1 { parent } else { std::process::id() };
    let (domain, stop_first) = match loaded {
        // launchd runs this runner's supervisor: unloading the job stops it.
        Some((d, Some(pid))) if pid == parent => (d, None),
        Some((d, _)) => (d, Some(ours)),
        None => (domains.first().cloned().unwrap_or_else(|| "gui/0".to_owned()), Some(ours)),
    };
    let mut tried = Vec::new();
    for d in std::iter::once(domain).chain(domains.iter().cloned()) {
        if tried.contains(&d) {
            continue;
        }
        match reload_later(&d, &label, &plist, stop_first, state_dir) {
            Ok(()) => return Ok(true),
            Err(e) => tried.push(format!("{d}: {e:#}")),
        }
    }
    anyhow::bail!("launchd would not run the reload: {}", tried.join("; "))
}

/// Runners of this user for `state_dir` that launchd is not running (`job` is the one it is): a
/// supervisor an install whose load failed started from a terminal, say, or a runner its
/// supervisor left behind when launchd stopped the job (an orphan, its parent gone). Two runners of
/// one machine would share its state directory, so an install stops them first.
fn stray_runners(state_dir: &Path, job: &[u32]) -> Vec<u32> {
    let Ok(out) = Command::new("ps").args(["-x", "-o", "pid=", "-o", "ppid=", "-o", "command="]).stdin(Stdio::null()).stderr(Stdio::null()).output() else {
        return Vec::new();
    };
    strays_in(&String::from_utf8_lossy(&out.stdout), state_dir, job, std::process::id())
}

/// [`stray_runners`] in `ps -o pid=,ppid=,command=` output.
fn strays_in(ps: &str, state_dir: &Path, job: &[u32], me: u32) -> Vec<u32> {
    let ours = state_dir.canonicalize().unwrap_or_else(|_| state_dir.to_path_buf());
    let same = |args: &[String]| {
        let theirs = crate::upgrade::arg_value(args, "--state-dir").map_or_else(|| crate::config::default_data_dir().join("agent"), PathBuf::from);
        theirs.canonicalize().unwrap_or(theirs) == ours
    };
    ps.lines()
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            let pid: u32 = words.next()?.parse().ok()?;
            let ppid: u32 = words.next()?.parse().ok()?;
            let words: Vec<String> = words.map(str::to_owned).collect();
            if pid == me || job.contains(&pid) {
                return None;
            }
            // `<kiln> supervise [--exe <kiln>] -- agent …`
            if let Some(sep) = words.iter().position(|w| w == "--") {
                let (before, after) = (words.get(..sep)?, words.get(sep + 1..)?);
                if before.iter().any(|w| w == "supervise") && after.first().is_some_and(|w| w == "agent") && same(after) {
                    return Some(pid);
                }
            }
            // `<kiln> [--config=…] agent …` whose supervisor is gone.
            let sub = words.iter().skip(1).find(|w| !w.starts_with('-'));
            (ppid == 1 && sub.is_some_and(|w| w == "agent") && words.iter().all(|w| w != "supervise") && same(&words)).then_some(pid)
        })
        .collect()
}

fn launchd(i: &Install<'_>) -> anyhow::Result<bool> {
    let label = launchd_label(i.unit);
    let path = launchd_plist(i.system, i.unit);
    let log = i.state_dir.join("kiln.log");
    std::fs::create_dir_all(i.state_dir).with_context(|| format!("create {}", i.state_dir.display()))?;
    let launcher = match ensure_launcher(i.unit, i.bin) {
        Ok(l) => {
            say(format!("launcher    {} (macOS allows it onto the local network once; upgrades replace {})", l.display(), i.bin.display()));
            Some(l)
        }
        Err(e) => {
            say(format!("launcher    none ({e:#}): the job runs {} itself", i.bin.display()));
            None
        }
    };
    crate::util::write_secret(&path, launchd_body(i, &log, launcher.as_deref()).as_bytes(), 0o644)?;
    let kind = if i.system { "LaunchDaemon" } else { "LaunchAgent" };
    let domains = launchd_domains(i.system);
    let job: Vec<u32> = domains.iter().filter_map(|d| launchd_job(d, &label).flatten()).collect();
    let strays = if i.args.iter().skip_while(|a| *a != "--").nth(1).is_some_and(|a| a == "agent") { stray_runners(i.state_dir, &job) } else { Vec::new() };
    // Run from one of the runner's own terminals or agents, stopping it would stop this process
    // before it loads the job again: a one-shot job of its own does both instead.
    let inside_stray = strays.iter().copied().find(|pid| is_ancestor_of_me(*pid));
    let inside_job = job.iter().any(|pid| is_ancestor_of_me(*pid));
    if inside_job || inside_stray.is_some() {
        let others: Vec<u32> = strays.iter().copied().filter(|p| Some(*p) != inside_stray).collect();
        crate::os::stop_processes(&others, None, Duration::from_secs(30));
        let d = domains.iter().find(|d| launchd_job(d, &label).is_some()).or(domains.first()).cloned().unwrap_or_default();
        reload_later(&d, &label, &path, inside_stray, i.state_dir)?;
        say(format!("boot        launchd {kind} {} ({d}; loads in a moment, which ends this shell's runner)", path.display()));
        return Ok(true);
    }
    if !strays.is_empty() {
        say(format!("stopping    a runner launchd didn't start (pid {})", strays.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")));
        crate::os::stop_processes(&strays, None, Duration::from_secs(30));
    }
    // Unload an earlier install so the new plist and binary apply, and wait until launchd has let
    // go of it (an unload finishes after the job's processes have stopped, which takes the
    // supervisor's grace period at most), then load it (RunAtLoad starts it now).
    for d in &domains {
        if launchd_job(d, &label).is_some() {
            let _ = launchctl(&["bootout", &format!("{d}/{label}")]);
        }
    }
    let gone_by = std::time::Instant::now() + Duration::from_secs(40);
    while domains.iter().any(|d| launchd_job(d, &label).is_some()) && std::time::Instant::now() < gone_by {
        std::thread::sleep(Duration::from_millis(500));
    }
    let mut said = String::new();
    let load_by = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        for d in &domains {
            let _ = launchctl(&["enable", &format!("{d}/{label}")]);
            match launchctl(&["bootstrap", d, &path.display().to_string()]) {
                Ok(()) => {
                    say(format!("boot        launchd {kind} {} ({d})", path.display()));
                    return Ok(true);
                }
                Err(e) => said = e,
            }
        }
        if std::time::Instant::now() >= load_by {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    say(format!(
        "boot        launchd {kind} {} not loaded ({said}); it loads at next {}",
        path.display(),
        if i.system { "boot" } else { "login" }
    ));
    Ok(false)
}

// --------------------------------------------------------------------- Windows Task Scheduler

/// Quote one argument for a Windows command line (the rules `CommandLineToArgvW` undoes).
pub(crate) fn win_quote(s: &str) -> String {
    if !s.is_empty() && !s.contains([' ', '\t', '"']) {
        return s.to_owned();
    }
    let mut out = String::from("\"");
    let mut slashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

/// When a Windows Scheduled Task starts kiln's supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskStart {
    /// At boot, whether or not anyone signs in, and again at the user's sign-in (nothing to do
    /// while it runs; it starts what a fast-startup boot didn't). As the user, through Windows'
    /// S4U logon: Windows keeps no password, and kiln gets none of the user's network
    /// credentials (as over SSH with a key). Registering it takes administrator rights.
    Boot,
    /// At the user's sign-in, in their session. Takes no administrator rights.
    SignIn,
}

/// A Task Scheduler definition that runs `kiln supervise --detach -- …` at `start`, with no time
/// limit (`schtasks /SC ONLOGON` would stop it after 72 hours), on battery too, at normal
/// priority (the default 7 is below normal, and jobs would inherit it), never elevated.
fn task_xml(i: &Install<'_>, start: TaskStart) -> String {
    let user = xml_escape(&crate::os::user_principal());
    let arguments = xml_escape(&i.detach_command_line());
    let sign_in = format!("    <LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>{user}</UserId>\n    </LogonTrigger>\n");
    let (when, triggers, logon_type) = match start {
        TaskStart::Boot => (
            "at boot, whether or not anyone signs in",
            format!("    <BootTrigger>\n      <Enabled>true</Enabled>\n    </BootTrigger>\n{sign_in}"),
            "S4U",
        ),
        TaskStart::SignIn => ("at sign-in", sign_in, "InteractiveToken"),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>kiln {unit}: starts kiln's own supervisor {when}</Description>
  </RegistrationInfo>
  <Triggers>
{triggers}  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>{logon_type}</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        unit = xml_escape(i.unit),
        command = xml_escape(&i.bin.display().to_string()),
    )
}

/// The Startup-folder fallback: a `.cmd` the shell runs at logon.
pub(crate) fn startup_script(unit: &str) -> PathBuf {
    crate::os::config_home()
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup")
        .join(format!("{unit}.cmd"))
}

/// Start kiln with a Scheduled Task, at boot when `start` asks for it and Windows lets this
/// install register that (administrator rights: this process's, or those Windows asks the person
/// here for), else at sign-in. Returns whether the task started kiln now.
fn scheduled_task(i: &Install<'_>, start: TaskStart) -> anyhow::Result<bool> {
    std::fs::create_dir_all(i.state_dir).with_context(|| format!("create {}", i.state_dir.display()))?;
    let user = crate::os::user_principal();
    let elevated = crate::os::elevated();
    let mut not_at_boot = None;
    if start == TaskStart::Boot {
        let created = if elevated {
            create_task(i, TaskStart::Boot, false)
        } else if i.ask {
            say("boot        Windows asks for administrator rights, once, to start kiln at boot (before anyone signs in)");
            create_task(i, TaskStart::Boot, true)
        } else {
            Err("that takes administrator rights".to_owned())
        };
        match created {
            Ok(()) => {
                say(format!("boot        Scheduled Task \"{}\" at boot, as {user}, whether or not anyone signs in", i.unit));
                return task_now(i);
            }
            Err(why) => not_at_boot = Some(why),
        }
    }
    match create_task(i, TaskStart::SignIn, false) {
        Ok(()) => say(format!("boot        Scheduled Task \"{}\" at {user}'s sign-in", i.unit)),
        // A task of this name registered with administrator rights, which only an administrator
        // can replace: kept when it starts this same kiln, as another would run beside it.
        Err(why) => match task_definition(i.unit) {
            Some(task) if task.runs(i) => say(format!("boot        Scheduled Task \"{}\" {} (kept: set up with administrator rights)", i.unit, task.when())),
            Some(_) => anyhow::bail!(
                "the Scheduled Task \"{}\" starts kiln differently, and only an administrator can change it ({why}): run this again in an administrator PowerShell",
                i.unit
            ),
            None => return startup_folder(i, &why),
        },
    }
    if let Some(why) = not_at_boot {
        say(if elevated {
            format!("            not at boot: {why}")
        } else {
            format!("            not at boot ({why}); for that, run once in an administrator PowerShell: {}", install_command(i))
        });
    }
    task_now(i)
}

/// Where `schtasks.exe` is (by its full path, as an administrator runs it).
fn schtasks_exe() -> PathBuf {
    std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from("schtasks.exe"), |root| PathBuf::from(root).join("System32").join("schtasks.exe"))
}

/// Register Scheduled Task `i.unit` (replacing one of that name) to start kiln at `start`: as an
/// administrator, Windows asking the person here first, when `elevate`; else with this
/// process's rights. Why it couldn't, otherwise.
fn create_task(i: &Install<'_>, start: TaskStart, elevate: bool) -> Result<(), String> {
    // schtasks reads task XML as UTF-16 (with a byte-order mark).
    let xml_path = i.state_dir.join(format!("{}.task.xml", i.unit));
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(task_xml(i, start).encode_utf16().flat_map(u16::to_le_bytes));
    let created = std::fs::write(&xml_path, &bytes).map_err(|e| format!("write {}: {e}", xml_path.display())).and_then(|()| {
        if elevate {
            create_task_elevated(i.unit, &xml_path, &bytes)
        } else {
            match Command::new("schtasks").args(["/Create", "/F", "/TN", i.unit, "/XML"]).arg(&xml_path).stdin(Stdio::null()).output() {
                Ok(o) if o.status.success() => Ok(()),
                Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_owned()),
                Err(e) => Err(format!("run schtasks: {e}")),
            }
        }
    });
    let _ = std::fs::remove_file(&xml_path);
    created
}

/// `schtasks /Create` as an administrator, from the definition at `xml_path`, held open
/// meanwhile so nothing can change it: what the administrator's schtasks reads is what this
/// process wrote (`written`), whatever else runs as this user.
fn create_task_elevated(unit: &str, xml_path: &Path, written: &[u8]) -> Result<(), String> {
    use std::io::Read as _;
    let mut held = crate::os::open_unchangeable(xml_path).map_err(|e| format!("open {}: {e}", xml_path.display()))?;
    let mut now = Vec::new();
    held.read_to_end(&mut now).map_err(|e| format!("read {}: {e}", xml_path.display()))?;
    if now != written {
        return Err(format!("{} changed after it was written", xml_path.display()));
    }
    let parameters = ["/Create", "/F", "/TN", unit, "/XML", &xml_path.display().to_string()].map(win_quote).join(" ");
    let code = crate::os::run_elevated(&schtasks_exe(), &parameters, Duration::from_secs(120)).map_err(|e| e.to_string())?;
    drop(held);
    if code == 0 { Ok(()) } else { Err(format!("schtasks exited with {code}")) }
}

/// Have Scheduled Task `i.unit` start kiln now (it launches a detached supervisor and exits;
/// nothing happens while one with the same arguments runs, and one with others is stopped), and
/// drop the Startup-folder fallback an earlier install may have left, which would start it a
/// second way.
fn task_now(i: &Install<'_>) -> anyhow::Result<bool> {
    let _ = std::fs::remove_file(startup_script(i.unit));
    start_replacing(i, || Ok(quiet(Command::new("schtasks").args(["/Run", "/TN", i.unit]))))
}

/// What a Scheduled Task runs, as Task Scheduler has it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TaskDefinition {
    pub(crate) command: String,
    pub(crate) arguments: String,
    pub(crate) at_boot: bool,
}

impl TaskDefinition {
    /// Whether it starts the supervisor `i` describes.
    fn runs(&self, i: &Install<'_>) -> bool {
        self.command.eq_ignore_ascii_case(&i.bin.display().to_string()) && self.arguments == i.detach_command_line()
    }

    fn when(&self) -> &'static str {
        if self.at_boot { "at boot" } else { "at sign-in" }
    }
}

/// Scheduled Task `unit`, if there is one.
pub(crate) fn task_definition(unit: &str) -> Option<TaskDefinition> {
    let out = Command::new("schtasks").args(["/Query", "/TN", unit, "/XML"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| parse_task(&String::from_utf8_lossy(&out.stdout))).flatten()
}

/// A task's definition from its XML (`schtasks /Query /XML`).
fn parse_task(xml: &str) -> Option<TaskDefinition> {
    let field = |tag: &str| xml.split(&format!("<{tag}>")).nth(1)?.split(&format!("</{tag}>")).next().map(crate::upgrade::xml_unescape);
    Some(TaskDefinition { command: field("Command")?, arguments: field("Arguments").unwrap_or_default(), at_boot: xml.contains("<BootTrigger") })
}

/// The last resort when Task Scheduler refuses: a `.cmd` in the user's Startup folder, which the
/// desktop runs at sign-in.
fn startup_folder(i: &Install<'_>, why: &str) -> anyhow::Result<bool> {
    let path = startup_script(i.unit);
    let body = format!(
        "@echo off\r\nrem kiln {}: start kiln's supervisor at logon (installed by `kiln install`).\r\nstart \"\" /B {} {}\r\n",
        i.unit,
        win_quote(&i.bin.display().to_string()),
        i.detach_command_line()
    );
    crate::util::write_secret(&path, body.as_bytes(), 0o644)?;
    say(format!("boot        Startup folder {} (Task Scheduler refused: {why})", path.display()));
    Ok(false)
}

/// The `kiln install` that sets this service up again (an enrolled agent needs no token), for
/// PowerShell: `kiln install --agent --server … --name …`.
fn install_command(i: &Install<'_>) -> String {
    // `supervise -- <subcommand> …`
    let mut rest = i.args.iter().skip_while(|a| *a != "--").skip(1);
    let mut words = vec!["kiln".to_owned(), "install".to_owned()];
    if rest.next().is_some_and(|sub| sub == "agent") {
        words.push("--agent".to_owned());
    } else {
        words.extend(["--on".to_owned(), "machine".to_owned()]);
    }
    words.extend(rest.map(|a| ps_quote(a)));
    words.join(" ")
}

/// Quote one argument for PowerShell, where a bare `a,b` would be a list.
fn ps_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

// ---------------------------------------------------------------------------------- uninstall

pub fn uninstall(agent: bool) -> anyhow::Result<()> {
    remove_service(agent);
    say("data and config were kept; delete them yourself if you want a clean slate");
    Ok(())
}

/// Stops kiln's service (`kiln-agent` for an agent) and removes how it starts at boot.
fn remove_service(agent: bool) {
    let unit = if agent { "kiln-agent" } else { "kiln" };
    if have("systemctl") {
        for user in [true, false] {
            let flag: &[&str] = if user { &["--user"] } else { &[] };
            let _ = Command::new("systemctl").args(flag).args(["disable", "--now", unit]).status();
        }
    }
    let mut paths = vec![
        home().join(format!(".config/systemd/user/{unit}.service")),
        home().join(format!(".config/autostart/{unit}.desktop")),
        PathBuf::from(format!("/etc/systemd/system/{unit}.service")),
    ];
    if cfg!(target_os = "macos") {
        let label = launchd_label(unit);
        for system in [false, true] {
            for d in launchd_domains(system) {
                let _ = quiet(Command::new("launchctl").args(["bootout", &format!("{d}/{label}")]));
            }
            paths.push(launchd_plist(system, unit));
        }
        paths.push(launcher_path(unit));
    }
    if cfg!(windows) {
        remove_task(unit);
        paths.push(startup_script(unit));
        let stopped = crate::os::supervise::stop_supervisors(if agent { "agent" } else { "server" });
        if stopped > 0 {
            say(format!("stopped {stopped} running supervisor(s)"));
        }
    }
    for p in paths {
        if p.exists() {
            match std::fs::remove_file(&p) {
                Ok(()) => say(format!("removed {}", p.display())),
                Err(e) => say(format!("could not remove {}: {e}", p.display())),
            }
        }
    }
}

/// Delete Scheduled Task `unit`. One registered with administrator rights (kiln's at boot) takes
/// them to delete as well: Windows asks the person here for them, else they're told how.
fn remove_task(unit: &str) {
    if task_definition(unit).is_none() {
        return;
    }
    if quiet(Command::new("schtasks").args(["/Delete", "/F", "/TN", unit])) {
        say(format!("removed Scheduled Task \"{unit}\""));
        return;
    }
    let by_hand = format!("in an administrator PowerShell run: schtasks /Delete /TN {unit} /F");
    if !crate::join::interactive() {
        say(format!("could not remove Scheduled Task \"{unit}\" (only an administrator can); {by_hand}"));
        return;
    }
    say(format!("Windows asks for administrator rights to remove Scheduled Task \"{unit}\""));
    let parameters = ["/Delete", "/F", "/TN", unit].map(win_quote).join(" ");
    match crate::os::run_elevated(&schtasks_exe(), &parameters, Duration::from_secs(120)) {
        Ok(0) => say(format!("removed Scheduled Task \"{unit}\"")),
        Ok(code) => say(format!("could not remove Scheduled Task \"{unit}\" (schtasks exited with {code}); {by_hand}")),
        Err(e) => say(format!("could not remove Scheduled Task \"{unit}\" ({e}); {by_hand}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_quoting() {
        assert_eq!(win_quote("agent"), "agent");
        assert_eq!(win_quote("a b"), "\"a b\"");
        assert_eq!(win_quote(""), "\"\"");
        assert_eq!(win_quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(win_quote(r"C:\Program Files\kiln\"), r#""C:\Program Files\kiln\\""#);
    }

    #[test]
    fn a_server_install_that_fails_stops_the_supervisor_it_started_and_no_other() {
        let p = |pid: u32, cmd: &str| (pid, cmd.split_whitespace().map(str::to_owned).collect::<Vec<_>>());
        let procs = [
            p(101, "/home/me/.local/bin/kiln supervise -- server"),
            p(102, "/home/me/.local/bin/kiln server"),
            p(103, "/home/me/.local/bin/kiln supervise -- agent --server http://h:7070"),
            p(104, "/usr/bin/env -- server"),
            p(105, "/home/me/.local/bin/kiln supervise -- server --other"),
        ];
        assert_eq!(running_in(&procs, &["server".to_owned()]), [101]);
    }

    #[test]
    fn a_server_goes_to_kubernetes_unless_told_otherwise_and_an_agent_stays_here() -> anyhow::Result<()> {
        #[derive(clap::Parser)]
        struct Install {
            #[command(flatten)]
            a: InstallArgs,
        }
        let on = |words: &[&str]| -> anyhow::Result<On> {
            use clap::Parser as _;
            Install::try_parse_from(std::iter::once("install").chain(words.iter().copied()))?.a.target()
        };
        assert_eq!(on(&[])?, On::Kubernetes);
        assert_eq!(on(&["--on", "docker"])?, On::Docker);
        assert_eq!(on(&["--on", "machine"])?, On::Machine);
        assert_eq!(on(&["--system"])?, On::Machine, "a system-wide install is one on this machine");
        assert_eq!(on(&["--agent", "--server", "http://h:7070"])?, On::Machine);
        assert!(on(&["--agent", "--on", "docker"]).is_err(), "an agent is installed here");
        assert!(on(&["--system", "--on", "kubernetes"]).is_err(), "--system is for this machine");
        Ok(())
    }

    #[test]
    fn runners_launchd_did_not_start_are_found() {
        let state = Path::new("/nonexistent/kiln-state");
        let ps = "  101     1 /Users/me/.local/bin/kiln supervise -- agent --server http://h:7070 --state-dir /nonexistent/kiln-state\n  \
                   102   101 /Users/me/.local/bin/kiln agent --server http://h:7070 --state-dir /nonexistent/kiln-state\n  \
                   103     1 /Users/me/.local/share/kiln/launcher/kiln-agent supervise --exe /Users/me/.local/bin/kiln -- agent --state-dir /nonexistent/kiln-state\n  \
                   104     1 /usr/local/bin/kiln supervise -- server\n  \
                   105     1 /Users/me/.local/bin/kiln supervise -- agent --state-dir /elsewhere\n  \
                   106     1 /Users/me/.local/bin/kiln supervise -- agent --state-dir /nonexistent/kiln-state\n  \
                   107     1 /Users/me/.local/bin/kiln --config=/c.toml agent --server http://h:7070 --state-dir /nonexistent/kiln-state\n  \
                   108   900 /Users/me/.local/bin/kiln reach mcp\n  \
                   109     1 /Users/me/.local/bin/kiln agent --state-dir /elsewhere\n";
        // 101 and 106: supervisors of this state directory; 103 too, unless launchd runs it; 107: a
        // runner its supervisor left behind. Never the runner under a supervisor (102), a server
        // (104), another machine's runner (105, 109), something else of kiln's (108), or this process.
        assert_eq!(strays_in(ps, state, &[], 0), [101, 103, 106, 107]);
        assert_eq!(strays_in(ps, state, &[103], 106), [101, 107]);
    }

    #[test]
    fn an_earlier_installs_runner_with_other_arguments_is_found() {
        let state = Path::new("/nonexistent/kiln/agent");
        let ours = ["agent", "--server", "http://h:7070", "--name", "box", "--labels", "a,b"].map(str::to_owned);
        let p = |pid: u32, cmd: &str| (pid, cmd.split(' ').map(str::to_owned).collect::<Vec<_>>());
        let procs = [
            p(101, "/k/kiln supervise -- agent --server http://h:7070 --name box --labels a,b"),
            p(102, "/k/kiln supervise -- agent --server http://h:7070 --name old --labels a"),
            p(103, "/k/kiln agent --server http://h:7070 --name old --labels a"),
            p(104, "/k/kiln supervise -- agent --server http://h:7070 --name box --labels a,b --instance 0199a2b3"),
            p(105, "/k/kiln supervise -- agent --name box --state-dir /elsewhere"),
            p(106, "/k/kiln supervise -- server"),
            p(107, "/k/launcher/kiln-agent supervise --exe /k/kiln -- agent --name old --state-dir /nonexistent/kiln/agent"),
            p(108, "/k/kiln install --agent --server http://h:7070 --name old"),
        ];
        // 102 and 107: this service with other arguments. Never the service as installed (101),
        // a runner (103), a runner set up beside it (104), another state directory (105), the
        // server (106), nor what isn't a supervisor (108).
        assert_eq!(stale_in(&procs, &ours, state), [102, 107]);
    }

    /// The process table for real: a stand-in for an earlier install's supervisor (a shell whose
    /// command line reads as one, on a state directory of its own) is found and stopped.
    #[test]
    fn an_earlier_installs_runner_is_found_and_stopped() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = dir.path().display().to_string();
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/c", "ping", "-n", "60", "127.0.0.1", ">nul", "&", "rem"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "sleep 60; :", "kiln"]);
            c
        };
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.args(["supervise", "--", "agent", "--name", "old", "--state-dir", &state]).stdin(Stdio::null()).stdout(Stdio::null()).spawn()?;
        let pid = child.id();
        // Waited for meanwhile: dead and unwaited, it would look alive to `stop` for its grace.
        let reaper = std::thread::spawn(move || child.wait());
        let ours = ["agent", "--name", "new", "--state-dir", &state].map(str::to_owned);
        let (stale, inside) = stale_supervisors(&ours);
        crate::os::supervise::stop(&[pid]);
        reaper.join().map_err(|_| anyhow::anyhow!("the reaper panicked"))??;
        assert_eq!((stale, inside), (vec![pid], false));
        Ok(())
    }

    /// launchd for real (macOS, by hand): a one-shot job reloads another and stops a process
    /// first. Only throwaway jobs and processes of its own.
    #[test]
    #[ignore = "drives this Mac's launchd"]
    fn a_reload_from_its_own_job_stops_and_reloads() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let uid = crate::os::uid();
        let domain = format!("gui/{uid}");
        let label = format!("io.github.hyper-light.kiln-reload-test-{}", std::process::id());
        let plist = dir.path().join(format!("{label}.plist"));
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string>\
             <key>ProgramArguments</key><array><string>/bin/sleep</string><string>600</string></array><key>RunAtLoad</key><true/></dict></plist>\n"
        );
        std::fs::write(&plist, body)?;
        launchctl(&["bootstrap", &domain, &plist.display().to_string()]).map_err(anyhow::Error::msg)?;
        let first = (0..50).find_map(|_| launchd_job(&domain, &label).flatten().or_else(|| {
            std::thread::sleep(Duration::from_millis(100));
            None
        }));
        let mut stray = Command::new("/bin/sleep").arg("600").spawn()?;
        let stray_pid = stray.id();
        let result = (|| -> anyhow::Result<()> {
            let first = first.ok_or_else(|| anyhow::anyhow!("the test job never ran"))?;
            reload_later(&domain, &label, &plist, Some(stray_pid), dir.path())?;
            // The stray is stopped, then the job is loaded again: running, under another pid.
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            loop {
                let stopped = stray.try_wait()?.is_some();
                let again = launchd_job(&domain, &label).flatten().filter(|p| *p != first);
                let helper_gone = launchd_job(&domain, &format!("{label}.reload")).is_none();
                if stopped && again.is_some() && helper_gone {
                    return Ok(());
                }
                anyhow::ensure!(std::time::Instant::now() < deadline, "stopped {stopped}, reloaded {again:?}, helper gone {helper_gone}");
                std::thread::sleep(Duration::from_millis(250));
            }
        })();
        let _ = stray.kill();
        let _ = stray.wait();
        let _ = launchctl(&["bootout", &format!("{domain}/{label}")]);
        let _ = launchctl(&["bootout", &format!("{domain}/{label}.reload")]);
        result
    }

    #[test]
    fn boot_definitions() -> anyhow::Result<()> {
        let args = vec!["supervise".to_owned(), "--".to_owned(), "agent".to_owned(), "--name".to_owned(), "box <1> & co".to_owned()];
        let i = Install {
            system: false,
            unit: "kiln-agent",
            bin: Path::new("/Users/me/.local/bin/kiln"),
            args: &args,
            config: Path::new("/Users/me/.config/kiln/kiln.toml"),
            state_dir: Path::new("/Users/me/.local/share/kiln/agent"),
            env: None,
            ask: false,
        };
        let plist = launchd_body(&i, Path::new("/tmp/kiln.log"), None);
        assert!(plist.contains("<string>io.github.hyper-light.kiln-agent</string>"));
        assert!(plist.contains("<string>supervise</string>\n    <string>--</string>\n    <string>agent</string>"));
        assert!(plist.contains("box &lt;1&gt; &amp; co"));
        // `install --agent --upgrade` reads it back on macOS.
        let e = crate::upgrade::parse_plist(&plist, "kiln-agent", false).ok_or_else(|| anyhow::anyhow!("not parsed"))?;
        assert_eq!((e.boot.as_str(), e.bin.as_path()), ("launchd", Path::new("/Users/me/.local/bin/kiln")));
        assert_eq!(e.args, ["agent", "--name", "box <1> & co"]);
        assert_eq!(e.launcher, None);
        assert_eq!(e.env, ["KILN_CONFIG=/Users/me/.config/kiln/kiln.toml".to_owned(), format!("PATH={}", std::env::var("PATH").unwrap_or_default())]);
        // From the unit's launcher: upgrades replace the binary it runs, never the launcher.
        let launcher = Path::new("/Users/me/.local/share/kiln/launcher/kiln-agent");
        let plist = launchd_body(&i, Path::new("/tmp/kiln.log"), Some(launcher));
        assert!(plist.contains(
            "<string>/Users/me/.local/share/kiln/launcher/kiln-agent</string>\n    <string>supervise</string>\n    <string>--exe</string>\n    <string>/Users/me/.local/bin/kiln</string>\n    <string>--</string>\n    <string>agent</string>"
        ));
        let e = crate::upgrade::parse_plist(&plist, "kiln-agent", false).ok_or_else(|| anyhow::anyhow!("not parsed"))?;
        assert_eq!(e.bin, Path::new("/Users/me/.local/bin/kiln"), "what an upgrade replaces");
        assert_eq!(e.launcher.as_deref(), Some(launcher));
        assert_eq!(e.args, ["agent", "--name", "box <1> & co"]);
        // Its environment carries over when the job is rewritten.
        let env = e.env.clone();
        let again = Install { env: Some(&env), ..i };
        assert_eq!(crate::upgrade::parse_plist(&launchd_body(&again, Path::new("/tmp/kiln.log"), Some(launcher)), "kiln-agent", false).map(|e| e.env), Some(env));
        for start in [TaskStart::Boot, TaskStart::SignIn] {
            let xml = task_xml(&i, start);
            assert!(xml.contains("<Arguments>supervise --detach -- agent --name &quot;box &lt;1&gt; &amp; co&quot;</Arguments>"));
            assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
            assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"), "never elevated");
            assert!(xml.contains("<LogonTrigger>"), "both start at sign-in");
            // What a later install reads back to tell whether a task it can't replace is this one.
            let read = parse_task(&xml).ok_or_else(|| anyhow::anyhow!("not parsed"))?;
            assert!(read.runs(&i));
            assert_eq!(read.at_boot, start == TaskStart::Boot);
        }
        // At boot: as the user without a session of theirs (S4U), which takes no password.
        let boot = task_xml(&i, TaskStart::Boot);
        assert!(boot.contains("<BootTrigger>") && boot.contains("<LogonType>S4U</LogonType>"));
        let sign_in = task_xml(&i, TaskStart::SignIn);
        assert!(!sign_in.contains("<BootTrigger>") && sign_in.contains("<LogonType>InteractiveToken</LogonType>"));
        let other = Install { args: &["supervise".to_owned(), "--".to_owned(), "agent".to_owned()], ..i };
        let read = parse_task(&boot).ok_or_else(|| anyhow::anyhow!("not parsed"))?;
        assert!(!read.runs(&other), "another kiln's task");
        Ok(())
    }

    /// Task Scheduler for real (Windows, by hand): a task at sign-in is registered, read back and
    /// removed; one at boot only with administrator rights (run it elevated too). Only a
    /// throwaway task of its own, never started.
    #[test]
    #[ignore = "drives this machine's Task Scheduler"]
    fn a_task_is_registered_read_back_and_removed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let unit = format!("kiln-test-{}", std::process::id());
        let args = ["supervise", "--", "agent", "--name", "box <1> & co"].map(str::to_owned);
        let bin = Path::new(r"C:\Windows\System32\whoami.exe");
        let i = Install { system: false, unit: &unit, bin, args: &args, config: Path::new("kiln.toml"), state_dir: dir.path(), env: None, ask: false };
        let result = (|| -> anyhow::Result<()> {
            create_task(&i, TaskStart::SignIn, false).map_err(anyhow::Error::msg)?;
            let read = task_definition(&unit).ok_or_else(|| anyhow::anyhow!("not registered"))?;
            anyhow::ensure!(read.runs(&i) && !read.at_boot, "read back as {read:?}");
            // What `kiln install --agent --upgrade` finds to replace.
            let found = crate::upgrade::find_existing(&unit).ok_or_else(|| anyhow::anyhow!("an upgrade finds no {unit}"))?;
            anyhow::ensure!(found.boot == "task" && found.bin == bin && found.args == ["agent", "--name", "box <1> & co"], "an upgrade finds {found:?}");
            match create_task(&i, TaskStart::Boot, false) {
                Ok(()) => {
                    anyhow::ensure!(crate::os::elevated(), "registered at boot without administrator rights");
                    let read = task_definition(&unit).ok_or_else(|| anyhow::anyhow!("not registered"))?;
                    anyhow::ensure!(read.runs(&i) && read.at_boot, "read back as {read:?}");
                }
                Err(e) => anyhow::ensure!(!crate::os::elevated(), "refused at boot to an administrator: {e}"),
            }
            Ok(())
        })();
        remove_task(&unit);
        anyhow::ensure!(task_definition(&unit).is_none(), "{unit} is still there");
        result
    }

    /// An upgrade for real (Windows, by hand): the binary a task starts is replaced while it runs,
    /// the running one moved aside, as `kiln install --agent --upgrade` does to a runner. Only a
    /// throwaway task and binary of its own (a copy of `ping`, standing in for the runner).
    #[test]
    #[ignore = "drives this machine's Task Scheduler"]
    fn an_upgrade_replaces_the_binary_a_task_starts_while_it_runs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let unit = format!("kiln-test-{}", std::process::id());
        let bin = dir.path().join("kiln.exe");
        std::fs::copy(r"C:\Windows\System32\PING.EXE", &bin)?;
        let args = ["supervise", "--", "agent", "--name", "box"].map(str::to_owned);
        let i = Install { system: false, unit: &unit, bin: &bin, args: &args, config: Path::new("kiln.toml"), state_dir: dir.path(), env: None, ask: false };
        create_task(&i, TaskStart::SignIn, false).map_err(anyhow::Error::msg)?;
        let mut runner = Command::new(&bin).args(["-n", "60", "127.0.0.1"]).stdout(Stdio::null()).spawn()?;
        let owned = unit.clone();
        let upgraded = crate::rt::block_on(async move { crate::upgrade::install_alongside(&owned, &|_: String| {}).await });
        let _ = runner.kill();
        let _ = runner.wait();
        remove_task(&unit);
        upgraded?;
        let me = std::fs::metadata(std::env::current_exe()?)?.len();
        anyhow::ensure!(std::fs::metadata(&bin)?.len() == me, "{} is still the old binary", bin.display());
        let aside = std::fs::read_dir(dir.path())?.flatten().any(|e| e.file_name().to_string_lossy().contains(".old-"));
        anyhow::ensure!(aside, "the running binary wasn't moved aside");
        Ok(())
    }

    /// The administrator prompt for real (Windows, by hand: say yes, twice): a task at boot is
    /// registered through it, and removed through it again.
    #[test]
    #[ignore = "asks for administrator rights"]
    fn a_task_at_boot_through_the_administrator_prompt() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let unit = format!("kiln-test-{}", std::process::id());
        let args = ["supervise", "--", "agent"].map(str::to_owned);
        let bin = Path::new(r"C:\Windows\System32\whoami.exe");
        let i = Install { system: false, unit: &unit, bin, args: &args, config: Path::new("kiln.toml"), state_dir: dir.path(), env: None, ask: true };
        create_task(&i, TaskStart::Boot, true).map_err(anyhow::Error::msg)?;
        let read = task_definition(&unit);
        remove_task(&unit);
        let read = read.ok_or_else(|| anyhow::anyhow!("not registered"))?;
        anyhow::ensure!(read.runs(&i) && read.at_boot, "read back as {read:?}");
        anyhow::ensure!(task_definition(&unit).is_none(), "{unit} is still there");
        Ok(())
    }

    #[test]
    fn the_install_to_run_as_an_administrator() {
        let args: Vec<String> = ["supervise", "--", "agent", "--server", "http://10.0.0.2:7070", "--name", "O'Brien box", "--labels", "baremetal,windows,x86_64"]
            .map(str::to_owned)
            .to_vec();
        let bin = Path::new(r"C:\Users\me\AppData\Local\Programs\kiln\kiln.exe");
        let (config, state) = (Path::new("kiln.toml"), Path::new("state"));
        let i = Install { system: false, unit: "kiln-agent", bin, args: &args, config, state_dir: state, env: None, ask: false };
        // PowerShell would take a bare `a,b,c` for a list, and `'` ends a quoted string.
        assert_eq!(
            install_command(&i),
            "kiln install --agent --server http://10.0.0.2:7070 --name 'O''Brien box' --labels 'baremetal,windows,x86_64'"
        );
        let server = ["supervise", "--", "server"].map(str::to_owned);
        assert_eq!(install_command(&Install { unit: "kiln", args: &server, ..i }), "kiln install --on machine");
    }
}
