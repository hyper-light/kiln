//! The single kiln binary: server, agent, supervisor, installer and CLI.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use kiln::{config, install, supervise};

#[derive(Parser)]
#[command(name = "kiln", version, about = "Self-hosted GitOps CI/CD for the hyper-light stack")]
struct Cli {
    /// Config file (default: $KILN_CONFIG, /etc/kiln/kiln.toml as root, else ~/.config/kiln/kiln.toml).
    #[arg(long, global = true, env = "KILN_CONFIG")]
    config: Option<PathBuf>,
    /// Log filter, e.g. `info`, `kiln=debug`.
    #[arg(long, global = true, env = "KILN_LOG", default_value = "info")]
    log: String,
    /// Log as JSON lines.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server (UI, API, scheduler, GitOps, QUIC agent transport).
    Server,
    /// Run an agent that executes jobs on this machine.
    Agent(AgentArgs),
    /// Keep a kiln process alive: restart on failure, reap zombies, survive as PID 1.
    Supervise {
        /// Detach into the background (logs go to <data>/kiln.log).
        #[arg(long)]
        detach: bool,
        /// The kiln subcommand to supervise, e.g. `-- server`.
        #[arg(last = true, required = true)]
        args: Vec<String>,
    },
    /// Shorthand for `supervise -- server`.
    Up,
    /// Install (binary, config, start on boot) — server by default, `--agent` for an agent.
    Install(install::InstallArgs),
    /// Remove boot integration (keeps data and config).
    Uninstall {
        #[arg(long)]
        agent: bool,
    },
    /// Print a one-time login link for the UI (no password typing) and try to open it.
    Login {
        #[arg(long)]
        print: bool,
    },
    /// Print the admin token.
    Token,
    /// Validate a pipeline file and show the planned jobs.
    Validate { file: PathBuf },
    /// Print the default configuration.
    DefaultConfig,
    /// Show server health and queue state.
    Status,
}

#[derive(clap::Args, Clone)]
struct AgentArgs {
    #[arg(long, env = "KILN_SERVER", default_value = "http://localhost:7070")]
    server: String,
    /// QUIC address (host:port); defaults to what the server advertised at enrollment.
    #[arg(long, env = "KILN_QUIC")]
    quic: Option<String>,
    #[arg(long, env = "KILN_JOIN_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[arg(long, env = "KILN_AGENT_NAME")]
    name: Option<String>,
    #[arg(long, value_delimiter = ',', env = "KILN_LABELS", default_value = "baremetal")]
    labels: Vec<String>,
    #[arg(long, env = "KILN_SLOTS")]
    slots: Option<u32>,
    #[arg(long, env = "KILN_WORK_DIR")]
    work_dir: Option<PathBuf>,
    #[arg(long, env = "KILN_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// Run one job then exit (Kubernetes pods).
    #[arg(long)]
    ephemeral: bool,
    /// The job this ephemeral agent was launched for.
    #[arg(long)]
    job: Option<String>,
    #[arg(long)]
    no_terminal: bool,
    #[arg(long, env = "KILN_SHELL")]
    shell: Option<String>,
}

fn init_tracing(filter: &str, json: bool) {
    let filter = tracing_subscriber::EnvFilter::try_new(filter)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_target(false);
    let _ = if json { builder.json().try_init() } else { builder.try_init() };
}

/// Panics are contained at task boundaries; the hook makes sure each one is logged loudly.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let msg = kiln::sup::panic_message(info.payload());
        tracing::error!(location = %loc, panic = %msg, "panic (contained; the task will be restarted)");
    }));
}

fn shutdown_on_signals(token: CancellationToken) {
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
            tracing::warn!("cannot install signal handlers");
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        token.cancel();
    });
}

fn out(s: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stdout(), "{s}");
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli.log, cli.json);
    install_panic_hook();
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("kiln").build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "cannot start runtime");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(real_main(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn real_main(cli: Cli) -> anyhow::Result<()> {
    let config_path = cli.config.clone().unwrap_or_else(config::default_config_path);
    // Make the resolved config path visible to children (supervise → server).
    let shutdown = CancellationToken::new();
    let _ = kiln::SHUTDOWN.set(shutdown.clone());
    match cli.cmd {
        Cmd::Server => {
            let cfg = config::Config::load(&config_path)?;
            shutdown_on_signals(shutdown.clone());
            kiln::server::run(cfg, shutdown).await
        }
        Cmd::Agent(a) => {
            shutdown_on_signals(shutdown.clone());
            let base = config::default_data_dir().join("agent");
            let state_dir = a.state_dir.unwrap_or_else(|| base.clone());
            let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
            let opts = kiln::agent::Opts {
                server_url: a.server,
                quic: a.quic,
                name: a.name.unwrap_or_else(config::Config::hostname),
                labels: a.labels,
                slots: a.slots.unwrap_or_else(|| u32::try_from((cpus / 4).max(1)).unwrap_or(1)),
                work_dir: a.work_dir.unwrap_or_else(|| state_dir.join("work")),
                state_dir,
                join_token: a.token,
                ephemeral: a.ephemeral,
                dedicated_job: a.job,
                terminal: !a.no_terminal && !a.ephemeral,
                shell: a.shell,
                credentials: None,
            };
            if opts.ephemeral {
                return kiln::agent::run(opts, shutdown).await;
            }
            // Persistent agents supervise their own main loop too.
            let reg = kiln::sup::Registry::default();
            let sd = shutdown.clone();
            let h = kiln::sup::spawn(&reg, "agent", shutdown.clone(), move || kiln::agent::run(opts.clone(), sd.clone()));
            let _ = h.await;
            Ok(())
        }
        Cmd::Supervise { detach, args } => {
            let cfg = config::Config::load(&config_path).unwrap_or_default();
            let state_dir = if args.first().is_some_and(|a| a == "agent") {
                config::default_data_dir().join("agent")
            } else {
                cfg.server.data_dir.clone()
            };
            let mut args = args;
            if cli.config.is_some() {
                args.insert(0, format!("--config={}", config_path.display()));
            }
            if detach {
                return supervise::detach(&args, &state_dir);
            }
            supervise::run(supervise::Options { args, state_dir }).await
        }
        Cmd::Up => {
            let cfg = config::Config::load(&config_path)?;
            supervise::run(supervise::Options { args: vec!["server".into()], state_dir: cfg.server.data_dir }).await
        }
        Cmd::Install(a) => install::install(a).await,
        Cmd::Uninstall { agent } => install::uninstall(agent),
        Cmd::Login { print } => {
            let cfg = config::Config::load(&config_path)?;
            let token = kiln::server::admin_token(&cfg)?;
            let port = cfg.server.http_addr.rsplit_once(':').map_or("7070", |(_, p)| p);
            let base = if cfg.server.http_addr.starts_with("0.0.0.0") || cfg.server.http_addr.starts_with("127.") {
                format!("http://localhost:{port}")
            } else {
                cfg.server.public_url.clone()
            };
            let url = kiln::api::otp_url(&token, &base);
            out(&url);
            if !print {
                let _ = std::process::Command::new("xdg-open")
                    .arg(&url)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
            Ok(())
        }
        Cmd::Token => {
            let cfg = config::Config::load(&config_path)?;
            out(&kiln::server::admin_token(&cfg)?);
            Ok(())
        }
        Cmd::Validate { file } => {
            let src = std::fs::read_to_string(&file)?;
            let p = kiln::pipeline::parse(&src)?;
            let mut ctx = kiln::pipeline::Context::default();
            ctx.set("git.sha", "0000000000000000000000000000000000000000");
            ctx.set("branch", "main");
            let jobs = kiln::pipeline::plan(&p, &ctx)?;
            out(&format!("{}: valid — {} job(s)", file.display(), jobs.len()));
            for j in jobs {
                out(&format!(
                    "  {:<40} runs-on [{}]  needs [{}]  {} step(s)",
                    j.display,
                    j.labels.join(", "),
                    j.needs.join(", "),
                    j.steps.len()
                ));
            }
            Ok(())
        }
        Cmd::DefaultConfig => {
            out(&config::Config::render_default());
            Ok(())
        }
        Cmd::Status => {
            let cfg = config::Config::load(&config_path)?;
            let token = kiln::server::admin_token(&cfg)?;
            let port = cfg.server.http_addr.rsplit_once(':').map_or("7070", |(_, p)| p);
            let url = format!("http://127.0.0.1:{port}/api/overview");
            let v: serde_json::Value = reqwest::Client::new()
                .get(url)
                .bearer_auth(token)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            out(&serde_json::to_string_pretty(&v)?);
            Ok(())
        }
    }
}
