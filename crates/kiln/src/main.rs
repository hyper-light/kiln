//! The single kiln binary: server, agent, supervisor, installer and CLI.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use kiln::rt::Shutdown;

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
        /// The kiln binary to run the subcommand with (default: this one). A launchd job's
        /// launcher, a copy of kiln that upgrades leave alone, runs the `kiln` they replace.
        #[arg(long)]
        exe: Option<PathBuf>,
        /// The kiln subcommand to supervise, e.g. `-- server`.
        #[arg(last = true, required = true)]
        args: Vec<String>,
    },
    /// Shorthand for `supervise -- server`.
    Up,
    /// Install (binary, config, start on boot) — server by default, `--agent` for an agent.
    Install(install::InstallArgs),
    /// Ask a kiln server on this network to take this machine as an agent (an admin approves it
    /// after comparing a verification code), then install the agent to start on boot.
    Join(kiln::join::JoinArgs),
    /// Run kiln here without installing it, as an outpost: it stays in this terminal until you stop
    /// it (Ctrl-C), starts on nothing, and takes no jobs. The machine's terminals, AI sessions and
    /// agents show up in kiln as a runner's do.
    Outpost(OutpostArgs),
    /// Remove boot integration (keeps data and config).
    Uninstall {
        #[arg(long)]
        agent: bool,
    },
    /// Print one-time sign-in links for the UI (this machine and the LAN) and open the local one.
    Login {
        /// Only print the links; do not open a browser.
        #[arg(long)]
        print: bool,
        #[command(flatten)]
        server: ServerArgs,
    },
    /// Print the admin token of the server this machine can reach.
    Token {
        #[command(flatten)]
        server: ServerArgs,
    },
    /// Set the sign-in password (any device can then sign in with it), or remove it.
    Passwd {
        /// Read the new password from stdin instead of prompting.
        #[arg(long)]
        stdin: bool,
        /// Remove the password (only the admin token and sign-in links will work).
        #[arg(long, conflicts_with = "stdin")]
        clear: bool,
        #[command(flatten)]
        server: ServerArgs,
    },
    /// Check workflows (GitHub's or kiln's), overlays and actions: each problem as
    /// `file:line:col: severity code: message`, then each workflow's jobs. Exits 1 on any error.
    Validate {
        /// Read every file as a workflow of this dialect instead of by where it is.
        #[arg(long, value_parser = ["gha", "kiln"])]
        dialect: Option<String>,
        /// Print JSON: one object per file.
        #[arg(long)]
        json: bool,
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Print the default configuration.
    DefaultConfig,
    /// Show server health and queue state.
    Status {
        #[command(flatten)]
        server: ServerArgs,
    },
    /// Start AI coding agents (claude, ...) from this kiln shell: they work in this directory and
    /// show up under this shell in kiln's UI.
    Spawn(kiln::harness::cli::SpawnArgs),
    /// List, stop, interrupt or message this kiln shell's agents.
    Agents(kiln::harness::cli::AgentsArgs),
    /// Run a command on another runner (`--on linux/amd64`, `--on <name>`): this repo is synced over
    /// first, output streams back, and the exit code is the command's.
    Run(kiln::reach::cli::RunArgs),
    /// The runners commands can run on, best first for what you need (`kiln runners linux`).
    Runners(kiln::reach::cli::RunnersArgs),
    /// Reach between runners: this runner's identity and pinned keys.
    Reach(kiln::reach::cli::ReachArgs),
    /// Deployments: apps, syncs, clusters, imports from Argo CD, the fleet benchmark.
    Cd(kiln::cd::cli::CdArgs),
    /// CI secrets: set, list (names only), delete, import a dotenv file, rotate the data key.
    Secrets(kiln::ci::cli::SecretsArgs),
    /// CI variables (the `vars` context): set, list, delete.
    Vars(kiln::ci::cli::VarsArgs),
    /// The CI OIDC issuer: status, keys to paste or publish, rotation.
    Oidc(kiln::ci::cli::OidcArgs),
    /// CI actions: the mirrored catalog, prefetching, the action policy.
    Actions(kiln::ci::cli::ActionsArgs),
    /// A benchmark tool's output as CI metrics, added to the step's `$KILN_METRICS`
    /// (criterion, hyperfine, gobench, pytest-benchmark, gbench, libtest, k6).
    Metrics(kiln::ci::cli::MetricsArgs),
    /// Workflows of this working tree: `kiln workflow run [WORKFLOW]` runs them here, or on a
    /// server's runners (`--server`), before you push.
    Workflow(kiln::ci::local::WorkflowArgs),
    /// Inside a CI leg: an integration's credentials, exchanged with the leg's ID token or
    /// stored, as variables (`--print-env`) or for a command (`--exec CMD…`).
    Auth(kiln::runner::auth::AuthArgs),
    /// The packages a tree's lockfiles pin, as an SBOM (SPDX 2.3 or CycloneDX 1.6).
    Sbom(kiln::ci::sbom::SbomArgs),
    /// The CI engine's runs: a list, or one with its legs.
    Runs(kiln::ci::cli::RunsArgs),
    /// A CI run's or leg's logs.
    Logs(kiln::ci::cli::LogsArgs),
    /// Start a workflow_dispatch run.
    Dispatch(kiln::ci::cli::DispatchArgs),
    /// A new attempt of a CI run: every job, the failed ones, or one job.
    Rerun(kiln::ci::cli::RerunArgs),
    /// Cancel a CI run.
    Cancel(kiln::ci::cli::CancelArgs),
    /// Where CI legs go, and why they wait.
    Routing(kiln::ci::cli::RoutingArgs),
    /// CI environments: list, create (with protection), delete, deployments.
    Env(kiln::ci::cli::EnvArgs),
    /// Approve (or reject) a run's deployments waiting for review.
    Approve(kiln::ci::cli::ApproveArgs),
    /// A kept CI leg: keep it longer, or release it.
    Debug(kiln::ci::cli::DebugArgs),
    /// People who sign in: add (with a sign-in link), list, link, sign out, remove.
    Users(kiln::authz::cli::UsersArgs),
    /// What users may do: roles and capabilities in scopes (`developer@org:acme`).
    Grants(kiln::authz::cli::GrantsArgs),
    /// The built-in roles and custom ones.
    Roles(kiln::authz::cli::RolesArgs),
    /// Personal access tokens and agent tokens: create (shown once), list, revoke, rotate.
    Tokens(kiln::authz::cli::TokensArgs),
    /// kiln's MCP server over stdio (code mode, deployments, skills), for local agents.
    Mcp {
        #[command(flatten)]
        server: ServerArgs,
        /// Print an MCP token for an agent with this label (calls are audited as `agent:<label>`)
        /// instead of serving: `claude mcp add --transport http kiln <url>/api/mcp --header
        /// "Authorization: Bearer $(kiln mcp --print-token my-agent)"`.
        #[arg(long)]
        print_token: Option<String>,
    },
}

/// Which server a CLI command talks to. By default kiln finds it: the cluster bootstrap's
/// credentials, then the local server; every admin token is checked against the server first.
#[derive(clap::Args, Clone, Default)]
struct ServerArgs {
    /// The server's URL, e.g. http://localhost:7070.
    #[arg(long, env = "KILN_URL")]
    url: Option<String>,
    /// The server's admin token.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true)]
    token: Option<String>,
}

impl ServerArgs {
    async fn resolve(&self, cfg: &config::Config) -> anyhow::Result<kiln::access::Target> {
        kiln::access::resolve(cfg, self.url.as_deref(), self.token.as_deref()).await
    }
}

/// `kiln outpost`: see [`Cmd::Outpost`].
#[derive(clap::Args)]
struct OutpostArgs {
    /// The server's URL.
    #[arg(long, env = "KILN_SERVER")]
    server: String,
    /// The server's one-time invite (Machines -> Add an outpost). Without one, the outpost asks to
    /// join, as `kiln join` does: it shows a verification code, and an admin approves it (Runners ->
    /// Add machines) once the codes match. Once it has its certificate it runs on that, and needs
    /// neither again.
    #[arg(long, env = "KILN_JOIN_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Its name in kiln (default: this machine's hostname).
    #[arg(long, env = "KILN_AGENT_NAME")]
    name: Option<String>,
    /// Where it keeps its certificate and files (default: <data>/outpost).
    #[arg(long, env = "KILN_STATE_DIR")]
    state_dir: Option<PathBuf>,
    #[arg(long, env = "KILN_SHELL")]
    shell: Option<String>,
}

#[derive(clap::Args, Clone)]
struct AgentArgs {
    /// The server's URL (default: the one `kiln join` stored, else http://localhost:7070).
    #[arg(long, env = "KILN_SERVER")]
    server: Option<String>,
    /// QUIC address (host:port); defaults to what the server advertised at enrollment.
    #[arg(long, env = "KILN_QUIC")]
    quic: Option<String>,
    #[arg(long, env = "KILN_JOIN_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[arg(long, env = "KILN_AGENT_NAME")]
    name: Option<String>,
    /// Labels jobs target with `runs-on` (default: the ones `kiln join` stored, else baremetal).
    #[arg(long, value_delimiter = ',', env = "KILN_LABELS")]
    labels: Vec<String>,
    /// Announce this agent on the local network; when it is not enrolled yet, find a kiln server
    /// there and ask to join it (an admin approves it with a verification code).
    #[arg(long)]
    discover: bool,
    #[arg(long, env = "KILN_SLOTS")]
    slots: Option<u32>,
    #[arg(long, env = "KILN_WORK_DIR")]
    work_dir: Option<PathBuf>,
    #[arg(long, env = "KILN_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// Run as another process of this machine beside its usual runner (set up by
    /// `kiln install --agent --upgrade`; files under <state>/instances/<id>).
    #[arg(long, env = "KILN_INSTANCE")]
    instance: Option<String>,
    /// Run one job then exit (Kubernetes pods).
    #[arg(long)]
    ephemeral: bool,
    /// The job this ephemeral agent was launched for.
    #[arg(long)]
    job: Option<String>,
    #[arg(long)]
    no_terminal: bool,
    /// Take no commands from other runners (reach); this runner can still send them.
    #[arg(long, env = "KILN_NO_REACH")]
    no_reach: bool,
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

fn shutdown_on_signals(token: Shutdown) {
    // Windows has no SIGTERM: a supervised child is asked to stop through its stdin.
    kiln::os::supervise::watch_supervisor(token.clone());
    let signals = match kiln::rt::Signals::new(&[kiln::rt::SIGTERM, kiln::rt::SIGINT]) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "cannot install signal handlers");
            return;
        }
    };
    // The subscription is kept for the life of the process: a second SIGTERM during a graceful
    // shutdown is absorbed instead of restoring the default action and killing us mid-cleanup.
    kiln::rt::spawn(async move {
        while let Some(sig) = signals.next().await {
            if token.is_cancelled() {
                tracing::info!(signal = sig, "already shutting down");
            }
            token.cancel();
        }
    });
}

/// Run an agent until shutdown. Persistent agents supervise their own main loop. One that
/// handed its terminals to its supervisor to take up a new binary exits with
/// [`kiln::supervise::HANDOVER_EXIT`], so the supervisor starts that binary at once.
async fn run_agent(opts: kiln::agent::Opts, shutdown: Shutdown) -> anyhow::Result<()> {
    if opts.ephemeral {
        return kiln::agent::run(opts, shutdown).await;
    }
    let reg = kiln::sup::Registry::default();
    let sd = shutdown.clone();
    let h = kiln::sup::spawn(&reg, "agent", shutdown, move || kiln::agent::run(opts.clone(), sd.clone()));
    let _ = h.await;
    if kiln::agent::handed_over() {
        return Err(kiln::reach::cli::ExitWith(kiln::supervise::HANDOVER_EXIT).into());
    }
    Ok(())
}

fn out(s: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stdout(), "{s}");
}

/// A file's path as its repository sees it (from `.github/` or `.kiln/` on), which decides what
/// it is (docs/ci.md W02-R01); any other path as given.
fn repo_path(file: &std::path::Path) -> String {
    let s = format!("/{}", file.to_string_lossy().replace('\\', "/"));
    let at = ["/.github/", "/.kiln/"].iter().filter_map(|m| s.rfind(m)).max();
    at.and_then(|i| s.get(i + 1..)).unwrap_or(s.trim_start_matches('/')).trim_start_matches("./").to_owned()
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // `kiln workflow run` runs a server of its own in this process: its logs are the runs', and
    // the server says only what goes wrong unless asked for more.
    let quiet = matches!(cli.cmd, Cmd::Workflow(_)) && cli.log == "info" && std::env::var_os("KILN_LOG").is_none();
    init_tracing(if quiet { "warn" } else { &cli.log }, cli.json);
    install_panic_hook();
    // Everything runs on hyper-rt, hyper-raft's thread-per-core runtime, sized for the role.
    kiln::rt::configure(match &cli.cmd {
        Cmd::Server | Cmd::Up | Cmd::Workflow(_) => kiln::rt::Profile::server(),
        Cmd::Agent(_) | Cmd::Join(_) | Cmd::Outpost(_) => kiln::rt::Profile::agent(),
        _ => kiln::rt::Profile::tool(),
    });
    match kiln::rt::block_on(real_main(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        // `kiln run`: the command's own exit status, nothing more to say.
        Err(ref e) if let Some(kiln::reach::cli::ExitWith(code)) = e.downcast_ref::<kiln::reach::cli::ExitWith>() => ExitCode::from(*code),
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn real_main(cli: Cli) -> anyhow::Result<()> {
    let config_path = cli.config.clone().unwrap_or_else(config::default_config_path);
    // Make the resolved config path visible to children (supervise → server).
    let shutdown = Shutdown::new();
    let _ = kiln::SHUTDOWN.set(shutdown.clone());
    match cli.cmd {
        Cmd::Server => {
            let cfg = config::Config::load(&config_path)?;
            shutdown_on_signals(shutdown.clone());
            kiln::server::run(cfg, shutdown).await
        }
        Cmd::Agent(a) => {
            // What the supervisor handed this process (its handover channel), before anything
            // here starts a process that could inherit it.
            kiln::os::supervise::claim_inherited();
            shutdown_on_signals(shutdown.clone());
            let base = config::default_data_dir().join("agent");
            let state_dir = a.state_dir.clone().unwrap_or_else(|| base.clone());
            let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
            // What `kiln join` stored stands in for flags that were not given.
            let mut server = a.server.clone().or_else(|| kiln::join::stored(&state_dir, "server"));
            let mut quic = a.quic.clone();
            let mut name = a.name.clone();
            let mut labels = if a.labels.is_empty() {
                kiln::join::stored(&state_dir, "labels")
                    .map(|l| l.split(',').map(str::to_owned).collect())
                    .unwrap_or_else(|| vec!["baremetal".to_owned()])
            } else {
                a.labels.clone()
            };
            let announcer = (a.discover && !a.ephemeral).then(|| {
                let ca = std::fs::read_to_string(state_dir.join("ca.pem")).ok().and_then(|p| kiln::join::pem_der(&p).ok());
                let cfg = config::Config::load(&config_path).unwrap_or_default();
                kiln::discovery::AgentAnnouncer::start(&state_dir, ca.as_deref(), cfg.discovery, shutdown.clone())
            });
            if let Some(ann) = &announcer
                && a.token.is_none()
                && !kiln::join::enrolled(&state_dir)
            {
                let url = match server.clone() {
                    Some(s) => s,
                    None => {
                        tracing::info!("not enrolled yet: looking for kiln servers on this network");
                        let found = kiln::join::servers_in(&ann.table, std::time::Duration::from_secs(5)).await;
                        let interactive = kiln::join::interactive();
                        if found.len() == 1 || (found.len() > 1 && interactive) {
                            kiln::join::choose(found, true, interactive, &shutdown).await?
                        } else {
                            // No server yet, or several and nobody to ask: wait until one invites
                            // this machine (or, never having heard more than one, the only one).
                            let say = |m: &str| tracing::info!("{m}");
                            kiln::discovery::invite::wait_for_invitation(&ann.table, &ann.id(), true, &shutdown, &say).await?
                        }
                    }
                };
                let me = kiln::join::Machine::this(name.clone(), &a.labels);
                let say = |m: &str| tracing::info!("{m}");
                let joined = kiln::join::request(&url, &me, &state_dir, &say, &shutdown).await?;
                ann.enrolled(&joined.ca_der);
                server = Some(joined.server);
                quic = Some(joined.quic);
                name = Some(joined.name);
                labels = joined.labels;
            }
            let opts = kiln::agent::Opts {
                server_url: server.unwrap_or_else(|| "http://localhost:7070".to_owned()),
                quic,
                name: name.unwrap_or_else(config::Config::hostname),
                labels,
                slots: a.slots.unwrap_or_else(|| u32::try_from((cpus / 4).max(1)).unwrap_or(1)),
                work_dir: a.work_dir.unwrap_or_else(|| state_dir.join("work")),
                state_dir,
                join_token: a.token,
                ephemeral: a.ephemeral,
                dedicated_job: a.job,
                terminal: !a.no_terminal && !a.ephemeral,
                job_terminal: !a.no_terminal,
                shell: a.shell,
                credentials: None,
                instance: a.instance,
                // Commands from other runners are a terminal's worth of access: not without one.
                reach: (!a.ephemeral).then_some(!a.no_reach && !a.no_terminal),
                outpost: false,
            };
            let result = run_agent(opts, shutdown).await;
            if let Some(ann) = announcer {
                ann.finish().await;
            }
            result
        }
        Cmd::Outpost(a) => {
            shutdown_on_signals(shutdown.clone());
            let state_dir = a.state_dir.unwrap_or_else(|| config::default_data_dir().join("outpost"));
            // An invite works once: with a certificate that still works, the outpost runs on it.
            let enrolled = std::fs::read_to_string(state_dir.join("agent.pem")).is_ok_and(|c| kiln::pki::still_valid(&c));
            tracing::info!("kiln outpost: nothing is installed; it runs in this terminal until you stop it (Ctrl-C)");
            let mut name = a.name.unwrap_or_else(config::Config::hostname);
            let token = a.token.filter(|_| !enrolled);
            // No certificate and no invite: ask to join, as `kiln join` does (an admin approves it
            // after comparing the code); the certificate it gets is kept for the next time.
            if !enrolled && token.is_none() {
                let me = kiln::join::Machine::this(Some(name.clone()), &[kiln::proto::OUTPOST_LABEL.to_owned()]);
                let joined = kiln::join::request(&a.server, &me, &state_dir, &kiln::join::say_stderr, &shutdown).await?;
                name = joined.name;
            }
            let opts = kiln::agent::Opts {
                server_url: a.server,
                quic: None,
                name,
                labels: vec![kiln::proto::OUTPOST_LABEL.to_owned()],
                slots: 0,
                work_dir: state_dir.join("work"),
                state_dir,
                join_token: token,
                ephemeral: false,
                dedicated_job: None,
                terminal: true,
                job_terminal: true,
                shell: a.shell,
                credentials: None,
                instance: None,
                // It runs no other machine's commands (reach), and for now sends none.
                reach: None,
                outpost: true,
            };
            run_agent(opts, shutdown).await
        }
        Cmd::Join(mut j) => {
            shutdown_on_signals(shutdown.clone());
            // Without --server: find one; several and no terminal wait for an invitation.
            let mut waiting = None;
            if j.server.is_none() {
                let cfg = config::Config::load(&config_path).unwrap_or_default();
                if let Some((url, w)) = kiln::discovery::invite::server_for_join(&j, &cfg.discovery, &shutdown).await? {
                    j.server = Some(url);
                    waiting = w;
                }
            }
            let joined = kiln::join::cli(j, shutdown.clone()).await;
            if let Some(w) = waiting {
                w.end().await;
            }
            match joined? {
                Some(opts) => run_agent(opts, shutdown).await,
                None => Ok(()),
            }
        }
        Cmd::Supervise { detach, exe, args } => {
            let cfg = config::Config::load(&config_path).unwrap_or_default();
            let state_dir = if args.first().is_some_and(|a| a == "agent") {
                // The agent's own state directory, when it has one of its own.
                kiln::upgrade::arg_value(&args, "--state-dir").map_or_else(|| config::default_data_dir().join("agent"), PathBuf::from)
            } else {
                cfg.server.data_dir.clone()
            };
            let mut args = args;
            if cli.config.is_some() {
                args.insert(0, format!("--config={}", config_path.display()));
            }
            if detach {
                return supervise::detach(&args, &state_dir, exe.as_deref());
            }
            supervise::run(supervise::Options { args, state_dir, exe }).await
        }
        Cmd::Up => {
            let cfg = config::Config::load(&config_path)?;
            // A machine that never ran a server may rather join the one already on the network.
            if kiln::join::offer_instead_of_server(&config_path, &cfg).await? {
                return Ok(());
            }
            supervise::run(supervise::Options { args: vec!["server".into()], state_dir: cfg.server.data_dir, exe: None }).await
        }
        Cmd::Install(a) => install::install(a).await,
        Cmd::Uninstall { agent } => install::uninstall(agent),
        Cmd::Login { print, server } => {
            let cfg = config::Config::load(&config_path).unwrap_or_default();
            let t = server.resolve(&cfg).await?;
            let url = kiln::api::otp_url(&t.token, &t.url);
            out(&format!("Sign in on this machine (valid 5 minutes):\n  {url}"));
            if let Some(public) = t.public_url.as_deref().filter(|p| !kiln::access::is_loopback(p) && p.trim_end_matches('/') != t.url) {
                out(&format!("Sign in from another device on the network (valid 5 minutes):\n  {}", kiln::api::otp_url(&t.token, public)));
            }
            out("Tip: choose a password in Settings (or run `kiln passwd`) to sign in from any device without a link.");
            if !print {
                kiln::os::open_url(&url);
            }
            Ok(())
        }
        Cmd::Token { server } => {
            let cfg = config::Config::load(&config_path).unwrap_or_default();
            out(&server.resolve(&cfg).await?.token);
            Ok(())
        }
        Cmd::Passwd { stdin, clear, server } => {
            let cfg = config::Config::load(&config_path).unwrap_or_default();
            let t = server.resolve(&cfg).await?;
            let client = kiln::http::client::Client::public()?;
            let endpoint = format!("{}/api/settings/password", t.url);
            if clear {
                let resp = client.request(kiln::http::client::Method::Delete, &endpoint).bearer(&t.token).send().await?;
                anyhow::ensure!(resp.is_success(), "the server answered HTTP {}", resp.status());
                out("Password sign-in removed. Use the admin token or `kiln login` links to sign in.");
                return Ok(());
            }
            let pw = if stdin {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line.trim_end_matches(['\r', '\n']).to_owned()
            } else {
                let a = rpassword::prompt_password("New sign-in password: ")?;
                let b = rpassword::prompt_password("Repeat it: ")?;
                anyhow::ensure!(a == b, "the two passwords differ; nothing changed");
                a
            };
            if let Err(why) = kiln::password::validate(&pw) {
                anyhow::bail!("password not accepted: {why}");
            }
            let resp = client.put(&endpoint).bearer(&t.token).json(&serde_json::json!({ "password": pw })).send().await?;
            if !resp.is_success() {
                let body = resp.bytes(64 << 10).await.unwrap_or_default();
                anyhow::bail!("the server refused the password: {}", String::from_utf8_lossy(&body));
            }
            out(&format!("Password set. Sign in at {} from any device.", t.public_url.as_deref().unwrap_or(&t.url)));
            Ok(())
        }
        Cmd::Validate { dialect, json, files } => {
            use kiln_ci::expr::Dialect;
            let dialect = dialect.map(|d| if d == "gha" { Dialect::Gha } else { Dialect::Kiln });
            let (mut errors, mut all) = (0usize, Vec::new());
            for file in &files {
                let src = std::fs::read_to_string(file).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
                let c = kiln_ci::wf::check(&repo_path(file), &src, dialect);
                let name = file.display().to_string();
                let errs = c.diagnostics.iter().filter(|d| d.is_error()).count();
                errors += errs;
                if json {
                    all.push(serde_json::json!({ "file": name, "check": c }));
                    continue;
                }
                for d in &c.diagnostics {
                    out(&d.render(&name));
                }
                let jobs = c.outline.as_ref().and_then(|o| o["jobs"].as_array()).cloned().unwrap_or_default();
                let dialect = if c.dialect == Dialect::Gha { "GitHub" } else { "kiln" };
                let shape = match (c.kind, &c.outline) {
                    ("workflow", Some(_)) => format!("{dialect} workflow, {} job(s)", jobs.len()),
                    ("workflow", None) => format!("{dialect} workflow"),
                    (kind, _) => kind.to_owned(),
                };
                let verdict = match (errs, c.diagnostics.len() - errs) {
                    (0, 0) => "valid".to_owned(),
                    (0, w) => format!("valid, {w} warning(s)"),
                    (e, w) => format!("{e} error(s), {w} warning(s)"),
                };
                out(&format!("{name}: {shape}: {verdict}"));
                for j in &jobs {
                    let list = |k: &str| j[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                    let legs = match &j["legs_estimate"] {
                        serde_json::Value::Number(n) => format!("{n} leg(s)"),
                        _ => "legs from a matrix expression".to_owned(),
                    };
                    let on = match j["uses"].as_str() {
                        Some(u) => format!("uses {u}"),
                        None => format!("runs-on [{}]", list("runs_on")),
                    };
                    out(&format!("  {:<32} {on}  needs [{}]  {} step(s)  {legs}", j["id"].as_str().unwrap_or_default(), list("needs"), j["steps"]));
                }
            }
            if json {
                out(&serde_json::to_string_pretty(&all)?);
            }
            if errors > 0 {
                anyhow::bail!("{errors} error(s)");
            }
            Ok(())
        }
        Cmd::DefaultConfig => {
            out(&config::Config::render_default());
            Ok(())
        }
        Cmd::Spawn(a) => kiln::harness::cli::spawn(a),
        Cmd::Agents(a) => kiln::harness::cli::agents(a),
        Cmd::Run(a) => {
            shutdown_on_signals(shutdown.clone());
            kiln::reach::cli::run(a, shutdown).await
        }
        Cmd::Runners(a) => kiln::reach::cli::runners(a),
        Cmd::Reach(a) => kiln::reach::cli::reach(a),
        Cmd::Cd(a) => {
            let cfg = config::Config::load(&config_path).unwrap_or_default();
            kiln::cd::cli::run(a, &cfg).await
        }
        Cmd::Secrets(a) => {
            let cfg = config::Config::load(&config_path).unwrap_or_default();
            kiln::ci::cli::secrets(a, &cfg).await
        }
        Cmd::Vars(a) => {
            let cfg = config::Config::load(&config_path).unwrap_or_default();
            kiln::ci::cli::vars(a, &cfg).await
        }
        Cmd::Oidc(a) => {
            let cfg = config::Config::load(&config_path).unwrap_or_default();
            kiln::ci::cli::oidc(a, &cfg).await
        }
        Cmd::Actions(a) => {
            let cfg = config::Config::load(&config_path).unwrap_or_default();
            kiln::ci::cli::actions(a, &cfg).await
        }
        Cmd::Runs(a) => kiln::ci::cli::runs(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Metrics(a) => kiln::ci::cli::metrics(a),
        Cmd::Auth(a) => kiln::runner::auth::cli(a).await,
        Cmd::Sbom(a) => kiln::ci::sbom::cli(a).await,
        Cmd::Workflow(a) => {
            // A signal stops the runs; the server and runner this process runs for them stop after.
            let stop = Shutdown::new();
            shutdown_on_signals(stop.clone());
            kiln::ci::local::workflow(a, &config::Config::load(&config_path).unwrap_or_default(), stop).await
        }
        Cmd::Logs(a) => kiln::ci::cli::logs(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Dispatch(a) => kiln::ci::cli::dispatch(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Rerun(a) => kiln::ci::cli::rerun(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Cancel(a) => kiln::ci::cli::cancel(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Routing(a) => kiln::ci::cli::routing(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Env(a) => kiln::ci::cli::env(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Approve(a) => kiln::ci::cli::approve(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Debug(a) => kiln::ci::cli::debug(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Users(a) => kiln::authz::cli::users(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Grants(a) => kiln::authz::cli::grants(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Roles(a) => kiln::authz::cli::roles(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Tokens(a) => kiln::authz::cli::tokens(a, &config::Config::load(&config_path).unwrap_or_default()).await,
        Cmd::Mcp { server, print_token } => {
            let cfg = config::Config::load(&config_path).unwrap_or_default();
            let t = server.resolve(&cfg).await?;
            match print_token {
                Some(label) => {
                    println!("{}", kiln::mcp::token(&t.token, &label));
                    Ok(())
                }
                None => kiln::mcp::stdio(t.url, t.token).await,
            }
        }
        Cmd::Status { server } => {
            let cfg = config::Config::load(&config_path).unwrap_or_default();
            let t = server.resolve(&cfg).await?;
            let resp = kiln::http::client::Client::public()?
                .get(&format!("{}/api/overview", t.url))
                .bearer(&t.token)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await?;
            if !resp.is_success() {
                anyhow::bail!("server answered HTTP {}", resp.status());
            }
            let v: serde_json::Value = resp.json(8 << 20).await?;
            out(&serde_json::to_string_pretty(&v)?);
            Ok(())
        }
    }
}
