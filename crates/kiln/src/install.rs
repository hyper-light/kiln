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
    /// Agent: join token (consumed during install; never stored in the boot command). It may start
    /// with `-` (random URL-safe base64): a value all the same.
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
fn bin_path(system: bool) -> PathBuf {
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
        reach: Some(true),
        observe: true,
        outpost: false,
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
fn launchd_label(unit: &str) -> String {
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

/// A launchd job running `kiln supervise -- …` at load (login, or boot for a daemon). launchd
/// restarts the supervisor only if it fails (a clean exit means it was stopped, or another
/// supervisor already runs the service); the supervisor restarts kiln itself.
fn launchd_body(i: &Install<'_>, log: &Path) -> String {
    let mut program = vec![i.bin.display().to_string()];
    program.extend(i.args.iter().cloned());
    let program: String = program.iter().map(|a| format!("\n    <string>{}</string>", xml_escape(a))).collect();
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_owned());
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
  <dict>
    <key>KILN_CONFIG</key>
    <string>{config}</string>
    <key>PATH</key>
    <string>{path}</string>
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
        config = xml_escape(&i.config.display().to_string()),
        path = xml_escape(&path),
        log = xml_escape(&log.display().to_string()),
    )
}

fn launchd(i: &Install<'_>) -> anyhow::Result<bool> {
    let label = launchd_label(i.unit);
    let path = launchd_plist(i.system, i.unit);
    let log = i.state_dir.join("kiln.log");
    std::fs::create_dir_all(i.state_dir).with_context(|| format!("create {}", i.state_dir.display()))?;
    crate::util::write_secret(&path, launchd_body(i, &log).as_bytes(), 0o644)?;
    let kind = if i.system { "LaunchDaemon" } else { "LaunchAgent" };
    // Unload an earlier install so the new plist and binary apply, then load it (RunAtLoad
    // starts it now). A bootout finishes asynchronously, so the bootstrap is retried briefly.
    let domains = launchd_domains(i.system);
    for d in &domains {
        let _ = quiet(Command::new("launchctl").args(["bootout", &format!("{d}/{label}")]));
    }
    for attempt in 0..3 {
        for d in &domains {
            let _ = quiet(Command::new("launchctl").args(["enable", &format!("{d}/{label}")]));
            if quiet(Command::new("launchctl").arg("bootstrap").arg(d).arg(&path)) {
                say(format!("boot        launchd {kind} {} ({d})", path.display()));
                return Ok(true);
            }
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    say(format!("boot        launchd {kind} {} (loads at next {})", path.display(), if i.system { "boot" } else { "login" }));
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
        let plist = launchd_body(&i, Path::new("/tmp/kiln.log"));
        assert!(plist.contains("<string>io.github.hyper-light.kiln-agent</string>"));
        assert!(plist.contains("<string>supervise</string>\n    <string>--</string>\n    <string>agent</string>"));
        assert!(plist.contains("box &lt;1&gt; &amp; co"));
        // `install --agent --upgrade` reads it back on macOS.
        let e = crate::upgrade::parse_plist(&plist, "kiln-agent", false).ok_or_else(|| anyhow::anyhow!("not parsed"))?;
        assert_eq!((e.boot.as_str(), e.bin.as_path()), ("launchd", Path::new("/Users/me/.local/bin/kiln")));
        assert_eq!(e.args, ["agent", "--name", "box <1> & co"]);
        let xml = task_xml(&i);
        assert!(xml.contains("<Arguments>supervise --detach -- agent --name &quot;box &lt;1&gt; &amp; co&quot;</Arguments>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        Ok(())
    }
}
