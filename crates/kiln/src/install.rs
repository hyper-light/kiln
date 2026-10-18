//! `kiln install`: put the binary in place, write config, arrange start-on-boot, start now.
//!
//! * **Interactive** (default) may ask for a sudo password only when `--system` is requested.
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
//! | Linux   | systemd unit, else crontab `@reboot`, else XDG autostart | `~/.local/bin` (`/usr/local/bin` as root) |
//! | macOS   | launchd: a LaunchAgent (a LaunchDaemon as root) | `~/.local/bin` (`/usr/local/bin` as root) |
//! | Windows | a per-user Scheduled Task at logon (no admin rights), else the Startup folder | `%LOCALAPPDATA%\Programs\kiln` |

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
    /// Windows: a Scheduled Task at logon (per user).
    Task,
    None,
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
    #[arg(long, env = "KILN_JOIN_TOKEN")]
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

fn say(msg: impl AsRef<str>) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "  {}", msg.as_ref());
}

/// Copy the running executable to `dest` atomically (no-op if it already is `dest`).
fn place_binary(dest: &Path) -> anyhow::Result<()> {
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

pub async fn install(a: InstallArgs) -> anyhow::Result<()> {
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
    eprintln_header(if a.agent { "Installing kiln agent" } else { "Installing kiln server" });
    place_binary(&bin)?;
    say(format!("binary      {}", bin.display()));

    let config_path = crate::config::default_config_path();
    let mut state_dir = crate::config::default_data_dir().join("agent");
    let (unit, args): (&str, Vec<String>) = if a.agent {
        ("kiln-agent", agent_setup(&a).await?)
    } else {
        if !config_path.exists() {
            let cfg = crate::config::Config::default();
            let text = format!(
                "# kiln configuration. Every key is optional; see docs/configuration.md.\n\n{}",
                toml::to_string_pretty(&cfg).unwrap_or_default()
            );
            crate::util::write_secret(&config_path, text.as_bytes(), 0o640)?;
            say(format!("config      {} (new)", config_path.display()));
        } else {
            say(format!("config      {} (kept)", config_path.display()));
        }
        let cfg = crate::config::Config::load(&config_path)?;
        let token = crate::server::admin_token(&cfg)?;
        say(format!("admin token {}", crate::server::token_path(&cfg).display()));
        let _ = token;
        state_dir = cfg.server.data_dir.clone();
        ("kiln", vec!["server".to_owned()])
    };

    let mut sup_args = vec!["supervise".to_owned(), "--".to_owned()];
    sup_args.extend(args);
    let boot = Install { system, unit, bin: &bin, args: &sup_args, config: &config_path, state_dir: &state_dir, env: None };
    let started_by_boot = setup_boot(a.boot, &boot)?;

    if !a.no_start && !started_by_boot {
        let mut cmd = Command::new(&bin);
        cmd.arg("supervise").arg("--detach").args(sup_args.iter().skip(1));
        run(&mut cmd)?;
        say("started     kiln supervise (detached)");
    }

    if !a.agent {
        let cfg = crate::config::Config::load(&config_path)?;
        let token = crate::server::admin_token(&cfg)?;
        let url = local_url(&cfg);
        say("");
        say(format!("UI          {url}"));
        say(format!("login link  {} (valid 5 min; `kiln login` makes a new one)", crate::api::otp_url(&token, &url)));
    }
    Ok(())
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
    let i = Install { system, unit, bin, args, config: &config, state_dir, env: (!env.is_empty()).then_some(env) };
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
}

impl Install<'_> {
    /// `supervise --detach -- <subcommand> …`: what launchers that do not supervise run.
    fn detach_args(&self) -> Vec<String> {
        let mut v = vec!["supervise".to_owned(), "--detach".to_owned()];
        v.extend(self.args.iter().skip(1).cloned());
        v
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
        Boot::Task => scheduled_task(i),
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
        let i = Install { system: existing.system, unit: &unit, bin: &existing.bin, args: &args, config: &config, state_dir, env };
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
fn win_quote(s: &str) -> String {
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

/// A Task Scheduler definition that runs `kiln supervise --detach -- …` at the user's logon,
/// with no time limit (`schtasks /SC ONLOGON` would stop it after 72 hours), on battery too, at
/// normal priority (the default 7 is below normal, and jobs would inherit it).
fn task_xml(i: &Install<'_>) -> String {
    let user = xml_escape(&crate::os::user_principal());
    let arguments = xml_escape(&i.detach_args().iter().map(|a| win_quote(a)).collect::<Vec<_>>().join(" "));
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>kiln {unit}: starts kiln's own supervisor at logon</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
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
fn startup_script(unit: &str) -> PathBuf {
    crate::os::config_home()
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup")
        .join(format!("{unit}.cmd"))
}

fn scheduled_task(i: &Install<'_>) -> anyhow::Result<bool> {
    std::fs::create_dir_all(i.state_dir).with_context(|| format!("create {}", i.state_dir.display()))?;
    // schtasks reads task XML as UTF-16 (with a byte-order mark).
    let xml_path = i.state_dir.join(format!("{}.task.xml", i.unit));
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(task_xml(i).encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&xml_path, bytes).with_context(|| format!("write {}", xml_path.display()))?;
    let created = Command::new("schtasks")
        .args(["/Create", "/F", "/TN", i.unit, "/XML"])
        .arg(&xml_path)
        .stdin(Stdio::null())
        .output();
    let _ = std::fs::remove_file(&xml_path);
    match created {
        Ok(o) if o.status.success() => {
            let _ = std::fs::remove_file(startup_script(i.unit));
            say(format!("boot        Scheduled Task \"{}\" (at logon, per user)", i.unit));
            // Start it now: the task only launches a detached supervisor and exits.
            Ok(quiet(Command::new("schtasks").args(["/Run", "/TN", i.unit])))
        }
        other => {
            let why = match other {
                Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_owned(),
                Err(e) => e.to_string(),
            };
            let path = startup_script(i.unit);
            let body = format!(
                "@echo off\r\nrem kiln {}: start kiln's supervisor at logon (installed by `kiln install`).\r\nstart \"\" /B {} {}\r\n",
                i.unit,
                win_quote(&i.bin.display().to_string()),
                i.detach_args().iter().map(|a| win_quote(a)).collect::<Vec<_>>().join(" ")
            );
            crate::util::write_secret(&path, body.as_bytes(), 0o644)?;
            say(format!("boot        Startup folder {} (Task Scheduler refused: {why})", path.display()));
            Ok(false)
        }
    }
}

// ---------------------------------------------------------------------------------- uninstall

pub fn uninstall(agent: bool) -> anyhow::Result<()> {
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
        if quiet(Command::new("schtasks").args(["/Delete", "/F", "/TN", unit])) {
            say(format!("removed Scheduled Task \"{unit}\""));
        }
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
    say("data and config were kept; delete them yourself if you want a clean slate");
    Ok(())
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
        let xml = task_xml(&i);
        assert!(xml.contains("<Arguments>supervise --detach -- agent --name &quot;box &lt;1&gt; &amp; co&quot;</Arguments>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        Ok(())
    }
}
