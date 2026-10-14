//! `kiln install`: put the binary in place, write config, arrange start-on-boot, start now.
//!
//! * **Interactive** (default) may ask for a sudo password only when `--system` is requested.
//! * **Unattended** (`--unattended`) never prompts: it installs per-user (no root needed), uses
//!   `sudo -n` for system installs (failing clearly if that would need a password), generates
//!   every secret itself and writes them `0600`.
//!
//! Boot integration only *launches* `kiln supervise`; restarting after failures is done by kiln's
//! own supervisor, so it works the same under systemd, cron, a desktop session or a container.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Boot {
    Auto,
    Systemd,
    Cron,
    Xdg,
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
    /// Agent: join token (consumed during install; never stored in the boot command).
    #[arg(long, env = "KILN_JOIN_TOKEN")]
    pub token: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long, value_delimiter = ',')]
    pub labels: Vec<String>,
    #[arg(long)]
    pub slots: Option<u32>,
}

fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn have(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
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
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = dest.with_extension(format!("new.{}", std::process::id()));
    std::fs::copy(&exe, &tmp).with_context(|| format!("copy to {}", tmp.display()))?;
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

pub async fn install(a: InstallArgs) -> anyhow::Result<()> {
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
    let system = a.system || is_root();
    let bin = if system { PathBuf::from("/usr/local/bin/kiln") } else { home().join(".local/bin/kiln") };
    eprintln_header(if a.agent { "Installing kiln agent" } else { "Installing kiln server" });
    place_binary(&bin)?;
    say(format!("binary      {}", bin.display()));

    let config_path = crate::config::default_config_path();
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
        ("kiln", vec!["server".to_owned()])
    };

    let mut sup_args = vec!["supervise".to_owned(), "--".to_owned()];
    sup_args.extend(args);
    let started_by_boot = setup_boot(&a, system, unit, &bin, &sup_args, &config_path)?;

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
        quic: None,
        name: name.clone(),
        labels: labels.clone(),
        slots: a.slots.unwrap_or(1),
        work_dir: state_dir.join("work"),
        state_dir: state_dir.clone(),
        join_token: a.token.clone(),
        ephemeral: false,
        dedicated_job: None,
        terminal: true,
        shell: None,
        credentials: None,
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
    if let Some(s) = a.slots {
        args.push("--slots".to_owned());
        args.push(s.to_string());
    }
    Ok(args)
}

/// Returns true when the boot mechanism also started the service now.
fn setup_boot(a: &InstallArgs, system: bool, unit: &str, bin: &Path, args: &[String], config: &Path) -> anyhow::Result<bool> {
    let systemd = have("systemctl") && Path::new("/run/systemd/system").exists();
    let boot = match a.boot {
        Boot::Auto if systemd => Boot::Systemd,
        Boot::Auto if have("crontab") => Boot::Cron,
        Boot::Auto if !system => Boot::Xdg,
        Boot::Auto => Boot::None,
        b => b,
    };
    let exec = format!(
        "{} {}",
        bin.display(),
        args.iter().map(|s| shell_quote(s)).collect::<Vec<_>>().join(" ")
    );
    match boot {
        Boot::Systemd => {
            let body = format!(
                "[Unit]\nDescription=kiln {unit} (self-supervising CI/CD job server)\nAfter=network-online.target\nWants=network-online.target\n\n\
                 [Service]\nType=simple\nEnvironment=KILN_CONFIG={cfg}\nExecStart={exec}\n\
                 # kiln supervises and restarts its own server; this only covers the supervisor itself.\n\
                 Restart=always\nRestartSec=5\nKillMode=mixed\nTimeoutStopSec=30\nLimitNOFILE=1048576\n\n\
                 [Install]\nWantedBy={target}\n",
                cfg = config.display(),
                target = if system { "multi-user.target" } else { "default.target" },
            );
            let (path, user_flag): (PathBuf, &[&str]) = if system {
                (PathBuf::from(format!("/etc/systemd/system/{unit}.service")), &[])
            } else {
                (home().join(format!(".config/systemd/user/{unit}.service")), &["--user"])
            };
            crate::util::write_secret(&path, body.as_bytes(), 0o644)?;
            run(Command::new("systemctl").args(user_flag).arg("daemon-reload"))?;
            run(Command::new("systemctl").args(user_flag).args(["enable", "--now", unit]))?;
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

pub fn uninstall(agent: bool) -> anyhow::Result<()> {
    let unit = if agent { "kiln-agent" } else { "kiln" };
    for user in [true, false] {
        let flag: &[&str] = if user { &["--user"] } else { &[] };
        let _ = Command::new("systemctl").args(flag).args(["disable", "--now", unit]).status();
    }
    let paths = [
        home().join(format!(".config/systemd/user/{unit}.service")),
        home().join(format!(".config/autostart/{unit}.desktop")),
        PathBuf::from(format!("/etc/systemd/system/{unit}.service")),
    ];
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
