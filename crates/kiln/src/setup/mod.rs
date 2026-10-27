//! `kiln setup`: kiln on this machine from start to finish, with a guide. It looks at the machine
//! (what's installed, Docker, a kubeconfig, kiln servers on the network, Tailscale, how a Mac signs
//! kiln), asks a few questions (or, `--unattended`, takes the flags and what it does on its own),
//! says everything it will do, and once the person says so, does it:
//!
//! - **Where kiln runs**: this machine by default, as a service started at boot (`kiln install
//!   --on machine`), whatever kubeconfig is here; or Docker, or Kubernetes; or as a runner of a
//!   kiln server (`--server`, with a join token or asking to join), as `install.sh` hands over.
//! - **Upgrades**: on a Mac, kiln's own code signature (`kiln signing setup`, docs/releases.md),
//!   without which every upgrade is cut off from the local network until someone allows it again.
//! - **The front door** (docs/front-door.md): DNS and HTTPS for kiln under a name of the person's
//!   (`kiln.internal` unless they choose another), on the tailnet too where Tailscale runs
//!   (docs/tailscale.md), with the one-time steps it needs root for on Linux (said first, sudo
//!   asked for itself, as `kiln runner setup` does), and a Tailscale key for kiln to keep the
//!   tailnet's DNS pointed here.
//! - **Signing in**: a password, for any device.
//!
//! What it finds done stays done, so it runs again whenever something's left. How it looks is
//! [`term`]'s: hyperlight's, opening with the shards mark ([`art`]).

mod art;
mod plan;
mod term;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde_json::Value;

use crate::frontdoor::{DnsMode, Settings};
use crate::install::{Boot, InstallArgs, On, Said};
use plan::{Answers, Facts, Level, Plan, Signing, Step, Target};
use term::{Choice, Interrupted, Mark, Term, Tone};

/// `kiln setup`.
#[derive(clap::Args, Debug, Clone)]
pub struct SetupArgs {
    /// Ask nothing: every answer from these flags and what setup does on its own (what a script
    /// runs). The front door then comes only with --domain, and root's steps only with --sudo.
    #[arg(long)]
    pub unattended: bool,
    /// Ask nothing (as --unattended), and joining, replace this machine's enrollment with
    /// another server's (as `kiln join --yes`).
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Say everything setup would do, and do none of it.
    #[arg(long)]
    pub dry_run: bool,
    /// Where a new kiln runs: this machine (the default, whatever kubeconfig is here), Docker on
    /// it, or Kubernetes (the kubeconfig's current context).
    #[arg(long, value_enum)]
    pub on: Option<On>,
    /// Make this machine a runner of the kiln server at this URL, rather than a kiln of its own.
    #[arg(long)]
    pub server: Option<String>,
    /// With --server: its join token (Runners → Add machines). Without one, this machine asks to
    /// join, and an admin approves it in kiln once the codes match.
    #[arg(long, env = "KILN_JOIN_TOKEN", hide_env_values = true, allow_hyphen_values = true)]
    pub token: Option<String>,
    /// With --server: this machine's name in kiln (default: its host name).
    #[arg(long)]
    pub name: Option<String>,
    /// With --server: the labels jobs target it with (`runs-on`).
    #[arg(long, value_delimiter = ',')]
    pub labels: Vec<String>,
    /// With --server: its job slots.
    #[arg(long)]
    pub slots: Option<u32>,
    /// With --server: the QUIC address to dial, rather than the one the server says.
    #[arg(long, env = "KILN_QUIC")]
    pub quic: Option<String>,
    /// How kiln starts on boot, as for `kiln install`.
    #[arg(long, value_enum, default_value = "auto")]
    pub boot: Boot,
    /// The front door's name: kiln answers DNS for it and serves HTTPS under it, from this
    /// machine (kiln.internal unless you choose).
    #[arg(long)]
    pub domain: Option<String>,
    /// Leave the front door as it is.
    #[arg(long, conflicts_with = "domain")]
    pub no_front_door: bool,
    /// How the front door's DNS answers.
    #[arg(long, value_enum)]
    pub dns: Option<Dns>,
    /// Keep the front door off the tailnet, though Tailscale runs here.
    #[arg(long)]
    pub no_tailnet: bool,
    /// On a Mac, don't give kiln its own code signature: every upgrade then waits for you to let
    /// kiln onto the local network again.
    #[arg(long)]
    pub no_signing: bool,
    /// Unattended, take the one-time steps the front door needs root for too (`sudo -n`): ports
    /// below 1024 on Linux, and ufw's rules. Asked about otherwise.
    #[arg(long)]
    pub sudo: bool,
    /// Don't open the sign-in link in a browser at the end.
    #[arg(long)]
    pub no_open: bool,
}

/// `--dns`: how the front door's DNS answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Dns {
    /// kiln's names only, for a router or a tailnet that sends them here (the default).
    Authoritative,
    /// Every name, for devices that use this machine as their DNS server.
    Resolver,
}

impl Dns {
    pub fn mode(self) -> DnsMode {
        match self {
            Dns::Authoritative => DnsMode::Authoritative,
            Dns::Resolver => DnsMode::Resolver,
        }
    }
}

/// What setup is given that's never shown or kept, apart from [`Answers`] (which are).
#[derive(Default)]
struct Secrets {
    tailnet_key: Option<String>,
    password: Option<String>,
}

/// What setup leaves for a person: what, and how.
type Left = Vec<(String, String)>;

/// `kiln setup`.
pub async fn cli(a: SetupArgs) -> anyhow::Result<()> {
    let asking = !(a.unattended || a.yes) && crate::join::interactive();
    let t = Term::detect(asking);
    // Ctrl-C, for as long as setup runs: what it waits on is let go of, and the terminal put back.
    let signals = crate::rt::Signals::new(&[crate::rt::SIGINT]).ok();
    let result = run(&a, t, asking, signals.as_ref()).await;
    crate::install::tell(None);
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.chain().any(|c| c.is::<Interrupted>()) => {
            t.line("");
            t.item(Mark::Skip, "Setup stopped", "what it finished stays done; `kiln setup` again takes up the rest");
            Err(crate::reach::cli::ExitWith(130).into())
        }
        Err(e) => {
            t.line("");
            t.item(Mark::Fail, "Setup didn't finish", &format!("{e:#}"));
            Err(crate::reach::cli::ExitWith(1).into())
        }
    }
}

async fn run(a: &SetupArgs, t: Term, asking: bool, signals: Option<&crate::rt::Signals>) -> anyhow::Result<()> {
    let f = term::opening(t, &side(&t), signals, look(a)).await?;
    let mut answers = plan::defaults(&f, a, asking);
    let mut secrets = Secrets::default();
    if asking {
        t.line("");
        t.para(2, Tone::Muted, "Setup asks a few questions, shows you what it will do, and does it once you say so. What it finds done stays done: run it again any time.");
        questions(t, a, &f, &mut answers, &mut secrets, signals).await?;
    }
    let p = plan::plan(&f, &answers);
    show(t, &f, &p);
    if a.dry_run {
        t.line("");
        t.item(Mark::Skip, "A dry run: nothing was done", "");
        return Ok(());
    }
    if !p.steps.is_empty() {
        if asking {
            t.line("");
            if !term::asking(signals, move || term::yes(t, "Go on?", true)).await? {
                t.item(Mark::Skip, "Nothing was done", "");
                return Ok(());
            }
        }
        t.part("Setting up");
    }
    let mut left: Left = p.notes.iter().filter(|n| n.level == Level::Left).map(|n| (n.text.clone(), n.detail.clone())).collect();
    let mut door = None;
    for step in &p.steps {
        take(t, a, &f, &secrets, step, asking, signals, &mut left, &mut door).await?;
    }
    finish(t, a, &f, &answers, &mut left, door.as_ref(), asking).await;
    Ok(())
}

/// The lines beside the shards mark: kiln's name, what it is, and this machine.
fn side(t: &Term) -> Vec<String> {
    let commit = crate::api::COMMIT.get(..7).unwrap_or_default();
    let version = if commit.is_empty() { format!("kiln {}", env!("CARGO_PKG_VERSION")) } else { format!("kiln {} · {commit}", env!("CARGO_PKG_VERSION")) };
    vec![
        format!("{}  {}", t.bold(&t.prism("kiln", 0.08)), t.muted("setup")),
        String::new(),
        "CI/CD, deployments and coding agents,".to_owned(),
        "on machines you own.".to_owned(),
        String::new(),
        t.faint(&format!("{} · {} {} · {version}", crate::config::Config::hostname(), plan::Os::this().name(), std::env::consts::ARCH)),
    ]
}

// ------------------------------------------------------------------------------------ the machine

/// What this machine has, as setup needs it: nothing here changes anything.
async fn look(a: &SetupArgs) -> Facts {
    let config_path = crate::config::default_config_path();
    let installed = config_path.exists();
    let cfg = crate::config::Config::load(&config_path).unwrap_or_default();
    let state = crate::config::default_data_dir().join("agent");
    let runner_of = if crate::join::enrolled(&state) { crate::join::stored(&state, "server") } else { None };
    // Servers to join are looked for only where nothing is decided yet.
    let discover = !installed && runner_of.is_none() && a.server.is_none() && a.on.is_none() && cfg.discovery.enabled;
    let interfaces = cfg.discovery.interfaces.clone();
    let (server, tailscale, docker, found, signing) = futures::join!(
        async {
            if installed { here(&cfg).await } else { None }
        },
        async { crate::rt::timeout(Duration::from_secs(6), crate::tailscale::local()).await.ok() },
        async { crate::rt::timeout(Duration::from_secs(6), crate::rt::blocking(docker_answers)).await.ok().and_then(Result::ok).unwrap_or(false) },
        async {
            if discover { crate::join::find_servers(Duration::from_secs(2), &interfaces).await.unwrap_or_default() } else { Vec::new() }
        },
        async { crate::rt::blocking(crate::os::macsign::state).await.ok() },
    );
    let tailscale = match tailscale {
        _ if crate::tailscale::command().is_none() => None,
        Some(Ok(l)) => Some(plan::Tailnet {
            usable: l.running() && l.tun,
            why: if !l.running() { format!("is {}", if l.backend.is_empty() { "not running" } else { l.backend.as_str() }) } else if !l.tun { "runs in userspace networking, where nothing can listen on its addresses".to_owned() } else { String::new() },
            address: l.primary().map(|a| a.to_string()),
            tailnet: l.tailnet,
            name: l.name,
        }),
        Some(Err(why)) => Some(plan::Tailnet { why: format!("doesn't answer ({why})"), ..plan::Tailnet::default() }),
        None => Some(plan::Tailnet { why: "doesn't answer".to_owned(), ..plan::Tailnet::default() }),
    };
    Facts {
        os: plan::Os::this(),
        host: crate::config::Config::hostname(),
        root: crate::os::is_root(),
        installed,
        server,
        runner_of,
        docker,
        cluster: crate::install_targets::current_context(),
        found: found.into_iter().filter_map(|i| Some(plan::Found { url: i.url()?, name: i.name, version: i.version })).collect(),
        signing: match signing {
            Some(crate::os::macsign::State::Ready(_)) => Signing::Ready,
            Some(crate::os::macsign::State::Missing(_)) => Signing::Missing,
            _ => Signing::NotNeeded,
        },
        tailscale,
        low_port: crate::frontdoor::netinfo::unprivileged_start(),
        ufw: crate::frontdoor::netinfo::ufw_enabled(),
        lans: crate::discovery::subnets(&[]),
        addresses: crate::frontdoor::netinfo::addresses(&crate::frontdoor::netinfo::lan()).iter().map(ToString::to_string).collect(),
    }
}

/// Where this machine's kiln server answers.
fn local_url(cfg: &crate::config::Config) -> String {
    let port = cfg.server.http_addr.rsplit_once(':').map_or("7070", |(_, p)| p);
    format!("http://localhost:{port}")
}

/// This machine's kiln server, when it answers: its runner's front door, whether it has a
/// Tailscale key and a password.
async fn here(cfg: &crate::config::Config) -> Option<plan::Server> {
    let url = local_url(cfg);
    let http = crate::http::client::Client::public().ok()?;
    let up = http.get(&format!("{url}/healthz")).timeout(Duration::from_secs(2)).send().await.is_ok_and(|r| r.is_success());
    if !up {
        return None;
    }
    let runner = cfg.local_agent.enabled.then(|| crate::pki::sanitize_name(&cfg.local_agent_name()));
    let mut server = plan::Server { url: url.clone(), runner: runner.clone(), password: crate::password::path(cfg).exists(), ..plan::Server::default() };
    if let Ok(admin) = crate::access::resolve(cfg, Some(&url), None).await {
        if let Some(r) = &runner
            && let Some(v) = get_json(&http, &format!("{url}/api/runners/{r}/front-door"), &admin.token).await
        {
            server.front_door = serde_json::from_value(at(&v, "/settings").clone()).ok();
        }
        server.tailnet_key = get_json(&http, &format!("{url}/api/tailscale"), &admin.token).await.is_some_and(|v| at(&v, "/configured").as_bool() == Some(true));
    }
    Some(server)
}

static NULL: Value = Value::Null;

/// `v` at the JSON pointer `p` (`/status/dns`), or `null` where there's nothing.
fn at<'a>(v: &'a Value, p: &str) -> &'a Value {
    v.pointer(p).unwrap_or(&NULL)
}

async fn get_json(http: &crate::http::client::Client, url: &str, token: &str) -> Option<Value> {
    let resp = http.get(url).bearer(token).timeout(Duration::from_secs(5)).send().await.ok().filter(crate::http::client::Response::is_success)?;
    resp.json(1 << 20).await.ok()
}

/// Whether Docker answers for this user.
fn docker_answers() -> bool {
    std::process::Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && !o.stdout.trim_ascii().is_empty())
}

/// `s` with this user's home as `~`.
pub(crate) fn home_short(s: &str) -> String {
    home_short_with(s, &crate::os::home_dir().display().to_string())
}

fn home_short_with(s: &str, home: &str) -> String {
    if home.len() < 2 {
        return s.to_owned();
    }
    s.replace(&format!("{home}/"), "~/").replace(&format!("{home}\\"), "~\\")
}

// ------------------------------------------------------------------------------------ questions

async fn questions(t: Term, a: &SetupArgs, f: &Facts, answers: &mut Answers, secrets: &mut Secrets, signals: Option<&crate::rt::Signals>) -> anyhow::Result<()> {
    where_it_runs(t, a, f, answers, signals).await?;
    if f.signing == Signing::Missing && !a.no_signing {
        t.part("Upgrades");
        t.para(
            2,
            Tone::Plain,
            "kiln upgrades itself: each new build starts beside the running one and takes over once it works. On a Mac that takes a code signature of kiln's own. macOS lets programs onto your local network by their signature, and kiln's builds are signed ad hoc, so to macOS every upgrade is a program it has never seen: cut off from the local network, and from your kiln server, until someone allows it again.",
        );
        t.line("");
        t.para(2, Tone::Muted, "Setup makes kiln a signing identity on this Mac (a certificate of its own, in a keychain of kiln's) and signs kiln with it, so every build is one program to macOS. macOS asks you once to trust the certificate.");
        t.line("");
        answers.sign = term::asking(signals, move || term::yes(t, "Give kiln its own signature?", true)).await?;
        if !answers.sign {
            t.item(Mark::Warn, "Upgrades here will wait for you", "`kiln signing setup` gives kiln its signature later");
        }
    }
    if answers.target.here() && !a.no_front_door {
        front_door_questions(t, a, f, answers, secrets, signals).await?;
    }
    if answers.target.here() && !f.server.as_ref().is_some_and(|s| s.password) {
        t.part("Signing in");
        t.para(2, Tone::Plain, "On this machine `kiln login` signs you in with a link. A password lets you sign in from any device, your phone too.");
        t.line("");
        if term::asking(signals, move || term::yes(t, "Choose a password?", true)).await? {
            match term::asking(signals, move || password(t)).await? {
                Some(pw) => {
                    secrets.password = Some(pw);
                    answers.password = true;
                }
                None => t.item(Mark::Skip, "No password for now", "`kiln passwd` sets one"),
            }
        }
    }
    Ok(())
}

/// Where kiln runs: a question only while that's open (no kiln here, nothing given).
async fn where_it_runs(t: Term, a: &SetupArgs, f: &Facts, answers: &mut Answers, signals: Option<&crate::rt::Signals>) -> anyhow::Result<()> {
    t.part("Where kiln runs");
    match &answers.target {
        Target::Here => {
            t.item(Mark::Done, "kiln runs on this machine", &f.server.as_ref().map(|s| s.url.clone()).unwrap_or_default());
            t.para(4, Tone::Muted, "Setup goes on from there, with what it has left to do.");
            return Ok(());
        }
        Target::Runner(server) => {
            t.item(Mark::Done, "This machine is a runner", &format!("of {server}"));
            t.para(4, Tone::Muted, "Setup sees to what this machine still needs. `kiln setup --on machine` would give it a kiln of its own instead.");
            return Ok(());
        }
        target @ (Target::Join { .. } | Target::New(_)) if a.server.is_some() || a.on.is_some() => {
            let step = match target {
                Target::Join { server, token } => Step::Join { server: server.clone(), token: *token },
                Target::New(on) => Step::Install(*on),
                _ => return Ok(()),
            };
            let (what, more) = plan::describe(f, &step);
            t.item(Mark::Todo, &what, &more);
            return Ok(());
        }
        _ => {}
    }
    let mut choices = vec![Choice::new("This machine", if f.installed { "kiln again: its config and data stay" } else { "a service that starts at boot · recommended" })];
    let mut targets = vec![Target::New(On::Machine)];
    if f.docker {
        choices.push(Choice::new("Docker on this machine", "a container, its data in a volume, on port 7070"));
        targets.push(Target::New(On::Docker));
    }
    if let Some(ctx) = &f.cluster {
        choices.push(Choice::new(format!("Kubernetes · {ctx}"), "the kubeconfig's current context, in namespace kiln"));
        targets.push(Target::New(On::Kubernetes));
    }
    for s in f.found.iter().take(3) {
        choices.push(Choice::new(format!("Join kiln on {}", s.name), format!("found on this network (kiln {}, {}): this machine runs its work", s.version, s.url)));
        targets.push(Target::Join { server: s.url.clone(), token: false });
    }
    if choices.len() == 1 {
        t.item(Mark::Todo, "This machine", "a service that starts at boot (`--on docker` or `--on kubernetes` put it elsewhere)");
        return Ok(());
    }
    t.line("  Where should kiln run?");
    t.line("");
    let at = term::asking(signals, move || term::choose(t, &choices, 0)).await?;
    answers.target = targets.into_iter().nth(at).unwrap_or(Target::New(On::Machine));
    if !answers.target.here() {
        // Another machine's runner, or a container's: its front door isn't this machine's.
        answers.door = None;
    }
    Ok(())
}

/// Names browsers open only over HTTPS they trust (the HSTS preload list has their whole
/// top-level domain): no way past a warning there.
fn preloaded(domain: &str) -> bool {
    ["dev", "app", "page", "new", "day", "foo", "zip", "mov", "bank", "insurance"].iter().any(|tld| domain.ends_with(&format!(".{tld}")))
}

async fn front_door_questions(t: Term, a: &SetupArgs, f: &Facts, answers: &mut Answers, secrets: &mut Secrets, signals: Option<&crate::rt::Signals>) -> anyhow::Result<()> {
    t.part("The front door");
    if let Some(d) = f.server.as_ref().and_then(|s| s.front_door.as_ref()).filter(|d| d.dns.enabled || d.https.enabled)
        && a.domain.is_none()
    {
        t.item(Mark::Done, &format!("The front door is on: {}", d.domain), &plan::door_summary(d));
        return Ok(());
    }
    t.para(
        2,
        Tone::Plain,
        "The front door makes this machine kiln's address under a name you choose: it answers DNS for the name, and serves kiln over HTTPS with a certificate from kiln's own root. Phones need HTTPS for voice input, and with Tailscale every device on your tailnet reaches kiln by that name, wherever it is.",
    );
    t.line("");
    if !term::asking(signals, move || term::yes(t, "Turn on the front door?", true)).await? {
        answers.door = None;
        t.item(Mark::Skip, "The front door stays off", "`kiln setup` again, or Runners → Domain & HTTPS, turns it on");
        return Ok(());
    }
    let default = a.domain.clone().unwrap_or_else(|| plan::DOMAIN.to_owned());
    let domain = term::asking(signals, move || term::ask(t, "Its name", &default, &crate::frontdoor::host_name)).await?;
    if preloaded(&domain) {
        t.para(4, Tone::Muted, "Browsers open a name like that only over HTTPS they trust, with no way past a warning: each device trusts kiln's root first (the end of setup says how).");
    }
    let mut door = plan::door(f, a, &domain);
    match &f.tailscale {
        Some(ts) if ts.usable && !a.no_tailnet => {
            let here = [ts.name.as_str(), ts.address.as_deref().unwrap_or_default()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
            t.item(Mark::Done, "Tailscale runs here", &here);
            let on = if ts.tailnet.is_empty() { String::new() } else { format!(" ({})", ts.tailnet) };
            door.tailnet = term::asking(signals, move || term::yes(t, &format!("Serve it on your tailnet{on} too?"), true)).await?;
            if door.tailnet && !f.server.as_ref().is_some_and(|s| s.tailnet_key) {
                t.line("");
                t.para(
                    4,
                    Tone::Muted,
                    &format!(
                        "kiln can point your tailnet's DNS for {domain} at this machine itself, with a Tailscale OAuth client: in Tailscale's admin console, Settings → Trust credentials, one with the DNS scope (write). Or later by hand: DNS → Nameservers → Add nameserver → Custom."
                    ),
                );
                if let Some(key) = term::asking(signals, move || tailnet_key(t)).await? {
                    secrets.tailnet_key = Some(key);
                    answers.tailnet_key = true;
                }
            }
        }
        Some(ts) if !ts.usable && !ts.why.is_empty() => t.item(Mark::Skip, "Not on your tailnet for now", &format!("Tailscale here {}: `kiln setup` again once it runs", ts.why)),
        None => t.item(Mark::Skip, "Not on a tailnet", "with Tailscale on this machine, `kiln setup` again serves the front door to your tailnet"),
        _ => {}
    }
    let steps = plan::root_steps(f, &plan::settings(&door));
    if !steps.is_empty() {
        t.line("");
        t.para(2, Tone::Plain, "kiln runs as you, and the front door needs root for a moment, once:");
        for (why, commands) in &steps {
            t.line(&format!("    {}", t.muted(why)));
            t.commands(&commands.iter().map(|c| c.shown(f.root)).collect::<Vec<_>>());
        }
        t.line("");
        let question = if f.root { "Run them now?" } else { "Run them now? sudo asks for your password." };
        answers.root_steps = term::asking(signals, move || term::yes(t, question, true)).await?;
    }
    answers.door = Some(door);
    Ok(())
}

/// An OAuth client's secret (or an API access token), checked as Tailscale's are written; `None`
/// when the person skips it.
fn tailnet_key(t: Term) -> anyhow::Result<Option<String>> {
    for _ in 0..3 {
        let key = term::secret(t, "Its secret (Enter to skip):")?;
        let key = key.trim();
        if key.is_empty() {
            return Ok(None);
        }
        match crate::tailscale::api::Credential::parse(key, "") {
            Ok(_) => return Ok(Some(key.to_owned())),
            Err(why) => t.line(&format!("    {} {}", t.mark(Mark::Fail), t.muted(&why))),
        }
    }
    Ok(None)
}

/// A new sign-in password, typed twice; `None` when the person skips it.
fn password(t: Term) -> anyhow::Result<Option<String>> {
    for _ in 0..3 {
        let pw = term::secret(t, "Password:")?;
        if pw.is_empty() {
            return Ok(None);
        }
        if let Err(why) = crate::password::validate(&pw) {
            t.line(&format!("    {} {}", t.mark(Mark::Fail), t.muted(&format!("{why}."))));
            continue;
        }
        if term::secret(t, "Again:")? == pw {
            return Ok(Some(pw));
        }
        t.line(&format!("    {} {}", t.mark(Mark::Fail), t.muted("The two differ.")));
    }
    Ok(None)
}

// ----------------------------------------------------------------------------------- the plan

fn mark(level: Level) -> Mark {
    match level {
        Level::Done => Mark::Done,
        Level::Left => Mark::Warn,
        Level::Info => Mark::Skip,
    }
}

fn show(t: Term, f: &Facts, p: &Plan) {
    t.part("The plan");
    let steps: Vec<(String, String)> = p.steps.iter().map(|s| plan::describe(f, s)).collect();
    let column = steps.iter().map(|(what, _)| what.as_str()).chain(p.notes.iter().map(|n| n.text.as_str())).map(|s| s.chars().count()).max().unwrap_or(0).clamp(20, 44) + 2;
    for (what, more) in &steps {
        t.item_in(Mark::Todo, what, more, column);
    }
    for n in &p.notes {
        t.item_in(mark(n.level), &n.text, &n.detail, column);
    }
}

// ----------------------------------------------------------------------------------- the steps

/// What installs say, shown setup's way for as long as this lives ([`install_line`]).
struct Told;

impl Told {
    fn start(t: Term, machine: bool) -> Told {
        let home = crate::os::home_dir().display().to_string();
        let skip = Arc::new(AtomicBool::new(false));
        let teller: crate::install::Teller = Arc::new(move |said: Said<'_>| {
            if let Some(line) = install_line(&t, &said, &home, machine, &skip) {
                t.line(&line);
            }
        });
        crate::install::tell(Some(teller));
        Told
    }
}

impl Drop for Told {
    fn drop(&mut self) {
        crate::install::tell(None);
    }
}

/// `line` as an install says a fact: its key, padded to column 12, then the value. A key is a word,
/// or one of the few of two words (so a sentence that starts with two short words isn't one).
fn keyed(line: &str) -> Option<(&str, &str)> {
    const TWO_WORDS: [&str; 5] = ["admin token", "login link", "not removed", "taking over", "in cluster"];
    let head = line.get(..12)?;
    let value = line.get(12..)?.trim_start();
    let key = head.trim_end();
    let word = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphabetic());
    let padded = head.get(key.len()..).is_some_and(|pad| !pad.is_empty() && pad.chars().all(|c| c == ' '));
    ((word || TWO_WORDS.contains(&key)) && padded && !value.is_empty()).then_some((key, value))
}

/// An install's line as setup shows it, under the step it belongs to: a `key  value` line's key
/// quiet in its column, a line that goes on from it under its value, the rest quieter; a heading
/// not at all (the step says what it is). For an install on this machine, where to open kiln and
/// its sign-in link (`UI`, `login link` and the line after) are what setup says at its end.
fn install_line(t: &Term, said: &Said<'_>, home: &str, machine: bool, skip: &AtomicBool) -> Option<String> {
    let Said::Line(line) = said else { return None };
    if line.trim().is_empty() {
        return None;
    }
    if line.trim_start().starts_with("Verification code:") {
        return Some(format!("    {}", t.bold(&t.prism(line.trim(), 0.2))));
    }
    if line.starts_with(' ') {
        if skip.load(Ordering::Relaxed) {
            return None;
        }
        return Some(format!("    {} {}", " ".repeat(12), t.muted(&home_short_with(line.trim(), home))));
    }
    skip.store(false, Ordering::Relaxed);
    if let Some((key, value)) = keyed(line) {
        if machine && (key == "UI" || key == "login link") {
            skip.store(true, Ordering::Relaxed);
            return None;
        }
        return Some(format!("    {} {}", t.faint(&format!("{key:<12}")), home_short_with(value, home)));
    }
    Some(format!("    {}", t.muted(&home_short_with(line.trim(), home))))
}

fn install_args(a: &SetupArgs, on: On, asking: bool) -> InstallArgs {
    InstallArgs {
        // At a terminal, Windows may ask for administrator rights to start kiln at boot.
        unattended: !asking,
        system: false,
        agent: false,
        boot: a.boot,
        no_start: false,
        server: None,
        quic: None,
        token: None,
        name: None,
        labels: Vec::new(),
        slots: None,
        upgrade: false,
        service: "kiln-agent".to_owned(),
        on: Some(on),
        context: None,
        namespace: "kiln".to_owned(),
        image: None,
        registry: None,
        port: 7070,
    }
}

/// Run `c` as root: itself when setup is root, else through sudo (`-n` unattended: it never asks
/// then, and fails instead). Its output is left out; what it says when it fails is the error.
fn run_root(c: &plan::Command, root: bool, unattended: bool) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::process::Stdio;
    let mut cmd = if root {
        std::process::Command::new(&c.program)
    } else {
        let mut sudo = std::process::Command::new("sudo");
        if unattended {
            sudo.arg("-n");
        }
        sudo.arg(&c.program);
        sudo
    };
    cmd.args(&c.args).stdout(Stdio::null()).stderr(Stdio::piped()).stdin(if c.input.is_some() { Stdio::piped() } else { Stdio::inherit() });
    let mut child = cmd.spawn().with_context(|| format!("run {}", c.shown(root)))?;
    if let (Some(input), Some(mut stdin)) = (&c.input, child.stdin.take()) {
        stdin.write_all(input.as_bytes()).with_context(|| format!("write to {}", c.shown(root)))?;
    }
    let out = child.wait_with_output()?;
    anyhow::ensure!(out.status.success(), "{} failed ({}): {}", c.shown(root), out.status, String::from_utf8_lossy(&out.stderr).trim());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn take(t: Term, a: &SetupArgs, f: &Facts, secrets: &Secrets, step: &Step, asking: bool, signals: Option<&crate::rt::Signals>, left: &mut Left, door: &mut Option<Value>) -> anyhow::Result<()> {
    match step {
        Step::Sign => {
            t.item(Mark::Todo, "Giving kiln its own signature", "if macOS asks to change your certificate trust settings, allow it");
            match term::unless_stopped(signals, crate::rt::blocking(crate::os::macsign::setup)).await?.and_then(|r| r) {
                Ok(signed) => t.item(Mark::Done, "kiln signs its builds on this Mac", &format!("{} binar{} signed", signed.len(), if signed.len() == 1 { "y" } else { "ies" })),
                Err(e) => {
                    t.item(Mark::Fail, "kiln isn't signed on this Mac", &format!("{e:#}"));
                    left.push(("Upgrades here wait for you".to_owned(), "`kiln signing setup`, at the Mac itself, gives kiln its signature".to_owned()));
                }
            }
        }
        Step::Root { why, commands } => {
            t.item(Mark::Todo, why, "");
            for c in commands {
                t.line(&format!("      {}", t.secondary(&c.shown(f.root))));
                let run = {
                    let (c, root, unattended) = (c.clone(), f.root, !asking);
                    move || run_root(&c, root, unattended)
                };
                if let Err(e) = term::unless_stopped(signals, crate::rt::blocking(run)).await?.and_then(|r| r) {
                    t.item(Mark::Fail, &format!("Not done: {}", why.to_lowercase()), &format!("{e:#}"));
                    left.push((why.clone(), commands.iter().map(|c| c.shown(f.root)).collect::<Vec<_>>().join("; ")));
                    return Ok(());
                }
            }
            t.item(Mark::Done, why, "done");
        }
        Step::Install(on) => {
            let (doing, done) = match on {
                On::Machine => ("Installing kiln on this machine", "kiln runs on this machine"),
                On::Docker => ("Installing kiln in Docker", "kiln runs in Docker"),
                On::Kubernetes => ("Installing kiln in Kubernetes", "kiln runs in Kubernetes"),
            };
            t.item(Mark::Todo, doing, "");
            let told = Told::start(t, *on == On::Machine);
            let installed = term::unless_stopped(signals, crate::install::install(install_args(a, *on, asking))).await;
            drop(told);
            installed?.context("installing kiln")?;
            t.item(Mark::Done, done, "");
        }
        Step::Join { server, token } => {
            t.item(Mark::Todo, &format!("Joining kiln at {server}"), "");
            let told = Told::start(t, false);
            let joined = if *token {
                let args = InstallArgs { agent: true, server: Some(server.clone()), quic: a.quic.clone(), token: a.token.clone(), name: a.name.clone(), labels: a.labels.clone(), slots: a.slots, on: None, ..install_args(a, On::Machine, asking) };
                term::unless_stopped(signals, crate::install::install(args)).await
            } else {
                let args = crate::join::JoinArgs { server: Some(server.clone()), yes: a.yes, name: a.name.clone(), labels: a.labels.clone(), slots: a.slots, no_install: false, boot: a.boot, wait: 3 };
                term::unless_stopped(signals, crate::join::cli(args, crate::rt::Shutdown::new())).await.map(|r| r.map(|_| ()))
            };
            drop(told);
            joined?.with_context(|| format!("joining {server}"))?;
            t.item(Mark::Done, "This machine is a runner", &format!("of {server}"));
        }
        Step::FrontDoor(settings) => match front_door(t, settings, signals).await {
            Ok(view) => {
                for (level, text, detail) in door_report(&view) {
                    if level == Level::Left {
                        left.push((text.clone(), detail.clone()));
                    }
                    t.item(mark(level), &text, &detail);
                }
                *door = Some(view);
            }
            Err(e) if e.chain().any(|c| c.is::<Interrupted>()) => return Err(e),
            Err(e) => {
                t.item(Mark::Fail, "The front door isn't on", &format!("{e:#}"));
                left.push(("Turn on the front door".to_owned(), "Runners → Domain & HTTPS, or `kiln setup` again".to_owned()));
            }
        },
        Step::TailnetKey => {
            let key = secrets.tailnet_key.clone().unwrap_or_default();
            match term::wait(t, "Asking Tailscale", signals, put("/api/tailscale", &serde_json::json!({ "secret": key }))).await? {
                Ok(v) => t.item(Mark::Done, "kiln keeps your tailnet's DNS pointed here", &at(&v, "/id").as_str().map(|id| format!("with OAuth client {id}")).unwrap_or_default()),
                Err(e) => {
                    t.item(Mark::Fail, "Tailscale didn't take the key", &format!("{e:#}"));
                    left.push(("Point your tailnet's DNS here".to_owned(), "a key in Runners → Domain & HTTPS, or the nameserver by hand (docs/tailscale.md)".to_owned()));
                }
            }
        }
        Step::Password => {
            let pw = secrets.password.clone().unwrap_or_default();
            match put("/api/settings/password", &serde_json::json!({ "password": pw })).await {
                Ok(_) => t.item(Mark::Done, "Your sign-in password is set", "any device can sign in with it"),
                Err(e) => {
                    t.item(Mark::Fail, "The password isn't set", &format!("{e:#}"));
                    left.push(("Choose a sign-in password".to_owned(), "`kiln passwd`, or Settings in kiln".to_owned()));
                }
            }
        }
    }
    Ok(())
}

/// This machine's kiln server, as its admin.
async fn admin() -> anyhow::Result<crate::access::Target> {
    let cfg = crate::config::Config::load(&crate::config::default_config_path())?;
    crate::access::resolve(&cfg, Some(&local_url(&cfg)), None).await
}

/// What an error response says: its JSON's `error`, else its text.
fn error_text(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| crate::util::truncate(String::from_utf8_lossy(body).trim(), 300).to_owned())
}

/// `PUT` `body` at `path` of this machine's kiln, as its admin: what it answers (`null` for
/// nothing).
async fn put(path: &str, body: &Value) -> anyhow::Result<Value> {
    let admin = admin().await?;
    let resp = crate::http::client::Client::public()?.put(&format!("{}{path}", admin.url)).bearer(&admin.token).json(body).timeout(Duration::from_secs(30)).send().await?;
    let status = resp.status();
    let bytes = resp.bytes(1 << 20).await.unwrap_or_default();
    anyhow::ensure!((200..300).contains(&status), "{} (HTTP {status})", error_text(&bytes));
    Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// The front door of this machine's kiln's runner, set to `settings`; then, once the runner has
/// taken them up (its listeners on, or failing, and its certificate in), how it is.
async fn front_door(t: Term, settings: &Settings, signals: Option<&crate::rt::Signals>) -> anyhow::Result<Value> {
    let cfg = crate::config::Config::load(&crate::config::default_config_path())?;
    anyhow::ensure!(cfg.local_agent.enabled, "this kiln runs no runner of its own (`[local_agent] enabled = false`): turn on a runner's front door in Runners → Domain & HTTPS");
    let runner = crate::pki::sanitize_name(&cfg.local_agent_name());
    t.item(Mark::Todo, &format!("Turning on the front door: {}", settings.domain), "");
    let path = format!("/api/runners/{runner}/front-door");
    put(&path, &serde_json::to_value(settings)?).await?;
    let admin = admin().await?;
    let http = crate::http::client::Client::public()?;
    let url = format!("{}{path}", admin.url);
    term::wait(t, "Waiting for it to come up", signals, async {
        let start = Instant::now();
        let mut last = Value::Null;
        loop {
            if let Some(v) = get_json(&http, &url, &admin.token).await {
                let done = settled(&v);
                last = v;
                if done {
                    return last;
                }
            }
            if start.elapsed() > Duration::from_secs(45) {
                return last;
            }
            crate::rt::sleep(Duration::from_millis(750)).await;
        }
    })
    .await
}

/// Whether the runner has taken up the front door's settings and settled: every part that's on
/// is on, or failing (not waiting), its certificate in.
fn settled(v: &Value) -> bool {
    if at(v, "/applied").as_bool() != Some(true) || at(v, "/status").is_null() {
        return false;
    }
    let waiting = |part: &str| !matches!(at(v, &format!("/status/{part}/state")).as_str(), Some("on" | "error"));
    let on = |part: &str| at(v, &format!("/settings/{part}/enabled")).as_bool() == Some(true);
    !((on("dns") && waiting("dns")) || (on("https") && (waiting("https") || waiting("cert"))) || (on("tailscale") && waiting("tailscale")))
}

/// A unix time as a day.
fn day(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix).ok().and_then(|t| t.format(&time::macros::format_description!("[year]-[month]-[day]")).ok()).unwrap_or_else(|| unix.to_string())
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

/// How the front door is, from its runner's report (`GET /api/runners/:name/front-door`): each
/// part that's on, and each that isn't working with what fixes it.
fn door_report(v: &Value) -> Vec<(Level, String, String)> {
    let mut out = Vec::new();
    let s = at(v, "/status");
    if s.is_null() || at(v, "/applied").as_bool() != Some(true) {
        out.push((Level::Left, "Its runner hasn't said how the front door is yet".to_owned(), "Runners → Domain & HTTPS shows it once it does".to_owned()));
        return out;
    }
    let domain = at(v, "/settings/domain").as_str().unwrap_or_default();
    let mut fixed: Vec<String> = Vec::new();
    let parts = [
        ("dns", "DNS", format!("DNS answers {domain}")),
        ("https", "HTTPS", format!("HTTPS serves https://{domain}")),
        ("http", "The redirect", "Plain HTTP sends browsers on to HTTPS".to_owned()),
        ("mdns", "The .local name", format!("{}.local answers on this network", at(v, "/settings/mdns/name").as_str().unwrap_or("kiln"))),
    ];
    for (key, what, on) in parts {
        let l = at(s, &format!("/{key}"));
        match at(l, "/state").as_str() {
            Some("on") => out.push((Level::Done, on, strings(at(l, "/on")).join(", "))),
            Some("error" | "waiting") => {
                let commands = strings(at(l, "/fix/commands"));
                fixed.extend(commands.iter().cloned());
                out.push((Level::Left, format!("{what}: {}", at(l, "/error").as_str().unwrap_or("not up yet")), commands.join("; ")));
            }
            _ => {}
        }
    }
    let cert = at(s, "/cert");
    match at(cert, "/state").as_str() {
        Some("on") => {
            let names = strings(at(cert, "/info/names")).join(", ");
            let until = at(cert, "/info/not_after").as_i64().map(|u| format!(" · until {}", day(u))).unwrap_or_default();
            let whose = if at(cert, "/kiln").as_bool() == Some(true) { "Its certificate is kiln's own" } else { "Its certificate is yours" };
            out.push((Level::Done, whose.to_owned(), format!("for {names}{until}")));
        }
        Some("error") => out.push((Level::Left, format!("Its certificate: {}", at(cert, "/error").as_str().unwrap_or("not issued")), String::new())),
        _ => {}
    }
    let ts = at(s, "/tailscale");
    match at(ts, "/state").as_str() {
        Some("on") => {
            let addresses = strings(at(ts, "/addresses")).join(", ");
            let name = at(ts, "/name").as_str().filter(|n| !n.is_empty()).map(|n| format!(" ({n})")).unwrap_or_default();
            out.push((Level::Done, format!("On your tailnet at {addresses}{name}"), "every device on it finds kiln by its name".to_owned()));
        }
        Some("error" | "waiting") => {
            let commands = strings(at(ts, "/fix/commands"));
            let detail = if commands.is_empty() { at(ts, "/fix/why").as_str().unwrap_or_default().to_owned() } else { commands.join("; ") };
            out.push((Level::Left, format!("Tailnet: {}", at(ts, "/error").as_str().unwrap_or("not up yet")), detail));
        }
        _ => {}
    }
    for fix in at(s, "/fixes").as_array().into_iter().flatten() {
        let commands = strings(at(fix, "/commands"));
        if commands.iter().all(|c| fixed.contains(c)) {
            continue;
        }
        fixed.extend(commands.iter().cloned());
        out.push((Level::Left, at(fix, "/why").as_str().unwrap_or("To do on this machine").to_owned(), commands.join("; ")));
    }
    out
}

/// Whether a browser can be opened here for the person.
fn desktop() -> bool {
    if std::env::var_os("SSH_CONNECTION").is_some() {
        return false;
    }
    !cfg!(target_os = "linux") || std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// The end: where kiln is and how to sign in, then what's left for the person.
async fn finish(t: Term, a: &SetupArgs, f: &Facts, answers: &Answers, left: &mut Left, door: Option<&Value>, asking: bool) {
    t.part("Ready");
    let kv = |key: &str, value: &str| t.line(&format!("  {} {value}", t.faint(&format!("{key:<14}"))));
    match &answers.target {
        Target::New(On::Machine) | Target::Here => match admin().await {
            Ok(admin) => {
                t.line(&format!("  {} on {}.", t.bold(&t.prism("kiln runs", 0.12)), f.host));
                t.line("");
                let link = crate::api::otp_url(&admin.token, &admin.url);
                kv("Sign in", &link);
                kv("", &t.faint("valid for 5 minutes · `kiln login` makes another"));
                if let Some(public) = admin.public_url.as_deref().filter(|p| !crate::access::is_loopback(p) && p.trim_end_matches('/') != admin.url) {
                    kv("Your network", public);
                }
                // A front door set before this run is shown too; what's left for one is said when it's set.
                let http = crate::http::client::Client::public().ok();
                let before = match (door, f.server.as_ref().and_then(|s| s.runner.as_ref()), &http) {
                    (None, Some(runner), Some(http)) => get_json(http, &format!("{}/api/runners/{runner}/front-door", admin.url), &admin.token).await,
                    _ => None,
                };
                if let Some(v) = door.or(before.as_ref()) {
                    let origins = strings(at(v, "/origins"));
                    if !origins.is_empty() {
                        kv("Front door", &origins.join(" · "));
                    }
                    if at(v, "/status/tailscale/state").as_str() == Some("on")
                        && let Some(first) = origins.first()
                    {
                        let tailnet = f.tailscale.as_ref().map(|ts| ts.tailnet.as_str()).filter(|n| !n.is_empty()).map(|n| format!(" on {n}")).unwrap_or_default();
                        kv("Your tailnet", &format!("{first}, from any device{tailnet}"));
                    }
                    if door.is_some() {
                        trust(v, f, left);
                    }
                }
                if asking && !a.no_open && desktop() {
                    crate::os::open_url(&link);
                    t.line("");
                    t.line(&format!("  {}", t.muted("The sign-in link is opening in your browser.")));
                }
            }
            Err(e) => t.item(Mark::Warn, "kiln doesn't answer here yet", &format!("{e:#}")),
        },
        Target::New(_) => t.item(Mark::Done, "kiln is up", "where to open it and how to sign in are above"),
        Target::Join { server, .. } | Target::Runner(server) => t.item(Mark::Done, &format!("This machine works for {server}"), "its terminals, agents and jobs show in kiln there"),
    }
    if f.os == plan::Os::Macos && answers.sign {
        left.push(("Allow kiln onto your local network, once".to_owned(), "when macOS asks about kiln finding devices on your local network: allow it, and upgrades keep it".to_owned()));
    }
    if !left.is_empty() {
        t.line("");
        for (text, detail) in left.iter() {
            t.item(Mark::Warn, text, detail);
        }
    }
    t.line("");
    t.line(&format!("  {}  {}", t.prism("━━━━", 0.3), t.muted("A little further into the light.")));
    t.line("");
}

/// What's left for the front door's HTTPS to be trusted, and its names found, on other devices.
fn trust(v: &Value, f: &Facts, left: &mut Left) {
    if at(v, "/settings/https/enabled").as_bool() != Some(true) {
        return;
    }
    let addresses = strings(at(v, "/status/addresses"));
    let lan = addresses.iter().chain(&f.addresses).find(|a| a.parse::<std::net::Ipv4Addr>().is_ok()).cloned();
    let fingerprint = at(v, "/ca/sha256").as_str().unwrap_or_default();
    let page = lan.as_ref().filter(|_| at(v, "/settings/https/http_port").as_u64() == Some(80)).map(|a| format!("on a phone, open http://{a} in Safari and follow the page; ")).unwrap_or_default();
    left.push(("Trust kiln's root on each device, once".to_owned(), format!("{page}elsewhere /ca.pem, /ca.crt or /ca.mobileconfig (docs/front-door.md) · SHA-256 {fingerprint}")));
    let domain = at(v, "/settings/domain").as_str().unwrap_or_default();
    if let Some(a) = &lan {
        match at(v, "/settings/dns/mode").as_str() {
            Some("authoritative") => left.push((format!("Send {domain} here from your router"), format!("conditional forwarding to {a}: with dnsmasq, `server=/{domain}/{a}` (docs/front-door.md has others)"))),
            Some("resolver") => left.push(("Point your devices' DNS here".to_owned(), format!("{a}, in your router's DHCP settings or on each device"))),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_installs_lines_are_shown_under_its_step() {
        let t = Term::plain();
        let skip = AtomicBool::new(false);
        let show = |said: Said<'_>, machine: bool| install_line(&t, &said, "/home/ada", machine, &skip);
        assert_eq!(show(Said::Line("binary      /home/ada/.local/bin/kiln"), true).as_deref(), Some("    binary       ~/.local/bin/kiln"));
        assert_eq!(show(Said::Line("admin token /home/ada/.local/share/kiln/admin.token"), true).as_deref(), Some("    admin token  ~/.local/share/kiln/admin.token"));
        assert_eq!(show(Said::Line("            for this terminal too, run: export PATH=/home/ada/.local/bin:$PATH"), true).as_deref(), Some("                 for this terminal too, run: export PATH=~/.local/bin:$PATH"));
        assert_eq!(show(Said::Heading("Installing kiln server"), true), None, "the step says what it is");
        assert_eq!(show(Said::Line(""), true), None);
        // Where to open it and how to sign in are setup's to say at its end, on this machine.
        assert_eq!(show(Said::Line("UI          http://localhost:7070"), true), None);
        assert_eq!(show(Said::Line("login link  http://localhost:7070/?otp=1 (valid 5 min)"), true), None);
        assert_eq!(show(Said::Line("            a new one: kiln login"), true), None);
        assert!(show(Said::Line("ready       it answers, its UI loads and its runner is connected"), true).is_some());
        // Elsewhere (a container, a cluster) the install says them.
        assert!(show(Said::Line("UI          http://localhost:7070"), false).is_some());
        assert_eq!(show(Said::Line("taking back the install, which didn't come up"), true).as_deref(), Some("    taking back the install, which didn't come up"));
        assert_eq!(show(Said::Line("    Verification code:  123 456"), false).as_deref(), Some("    Verification code:  123 456"));
        assert_eq!(keyed("taking back the install, which didn't come up"), None, "a sentence, not a key");
        assert_eq!(keyed("not removed /x (busy)"), Some(("not removed", "/x (busy)")));
        assert_eq!(keyed("container   kiln (kiln:abc)"), Some(("container", "kiln (kiln:abc)")));
        assert_eq!(keyed("UI"), None);
    }

    #[test]
    fn the_front_door_is_reported_from_its_runner() {
        let v = serde_json::json!({
            "applied": true,
            "settings": { "domain": "kiln.internal", "dns": { "enabled": true, "mode": "authoritative" }, "https": { "enabled": true, "http_port": 80 }, "mdns": { "enabled": true, "name": "kiln" }, "tailscale": { "enabled": true } },
            "status": {
                "addresses": ["192.168.1.77", "fd00::77"],
                "dns": { "state": "error", "error": "DNS cannot listen on port 53: ports below 1024 need root here", "fix": { "why": "ports", "commands": ["sudo sysctl -w net.ipv4.ip_unprivileged_port_start=53"] } },
                "https": { "state": "on", "on": ["[::]:443"] },
                "http": { "state": "on", "on": ["[::]:80"] },
                "mdns": { "state": "on", "on": ["kiln.local"] },
                "cert": { "state": "on", "kiln": true, "info": { "names": ["kiln.internal", "*.kiln.internal", "kiln.local"], "not_after": 1_824_681_600 } },
                "tailscale": { "state": "on", "addresses": ["100.64.0.5"], "name": "box.tail1.ts.net" },
                "fixes": [{ "why": "ports", "commands": ["sudo sysctl -w net.ipv4.ip_unprivileged_port_start=53"] }, { "why": "ufw is on here: let the LAN reach the front door.", "commands": ["sudo ufw allow from 192.168.1.0/24 to any port 53,80,443 proto tcp"] }]
            },
            "origins": ["https://kiln.internal", "https://kiln.local"],
            "ca": { "sha256": "AB:CD" }
        });
        assert!(settled(&v), "every part on or failing, its certificate in");
        let report = door_report(&v);
        let said: Vec<(Level, &str)> = report.iter().map(|(l, s, _)| (*l, s.as_str())).collect();
        assert_eq!(
            said,
            [
                (Level::Left, "DNS: DNS cannot listen on port 53: ports below 1024 need root here"),
                (Level::Done, "HTTPS serves https://kiln.internal"),
                (Level::Done, "Plain HTTP sends browsers on to HTTPS"),
                (Level::Done, "kiln.local answers on this network"),
                (Level::Done, "Its certificate is kiln's own"),
                (Level::Done, "On your tailnet at 100.64.0.5 (box.tail1.ts.net)"),
                (Level::Left, "ufw is on here: let the LAN reach the front door."),
            ],
            "a fix said once"
        );
        assert!(report.iter().any(|(_, _, d)| d == "for kiln.internal, *.kiln.internal, kiln.local · until 2027-10-28"), "{report:?}");
        // What's left for other devices: trusting the root, and the router's DNS.
        let mut left = Left::new();
        trust(&v, &Facts::default(), &mut left);
        assert_eq!(left.len(), 2);
        assert!(left.first().is_some_and(|(_, d)| d.starts_with("on a phone, open http://192.168.1.77 in Safari") && d.ends_with("SHA-256 AB:CD")), "{left:?}");
        assert!(left.get(1).is_some_and(|(s, d)| s == "Send kiln.internal here from your router" && d.contains("server=/kiln.internal/192.168.1.77")), "{left:?}");
        // Not yet: its certificate still waited for, or settings not taken up.
        let mut waiting = v.clone();
        if let Some(state) = waiting.pointer_mut("/status/cert/state") {
            *state = "waiting".into();
        }
        assert!(!settled(&waiting));
        let mut stale = v;
        if let Some(applied) = stale.pointer_mut("/applied") {
            *applied = false.into();
        }
        assert!(!settled(&stale));
        assert_eq!(door_report(&stale).first().map(|(l, _, _)| *l), Some(Level::Left));
    }

    #[test]
    fn the_install_scripts_hand_over_to_setup_and_an_older_kiln_installs_as_before() {
        let sh = include_str!("../../assets/install-agent.sh");
        assert!(sh.contains(r#"exec "$BIN_DIR/kiln" setup --server "$SERVER" $ARGS </dev/tty"#), "at a terminal: asked there");
        assert!(sh.contains(r#"exec "$BIN_DIR/kiln" setup --unattended --server "$SERVER" $ARGS"#), "without one: nothing asked");
        assert!(sh.contains(r#"exec "$BIN_DIR/kiln" install --agent --unattended --server "$SERVER" $ARGS"#), "a kiln from before setup");
        let ps1 = include_str!("../../assets/install-agent.ps1");
        assert!(ps1.contains("$setupArgs = @('setup', '--server', $Server)"));
        assert!(ps1.contains("$installArgs = @('install', '--agent', '--server', $Server)"), "a kiln from before setup");
    }

    #[test]
    fn setups_flags_read_as_install_sh_passes_them() -> anyhow::Result<()> {
        use clap::Parser as _;
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            a: SetupArgs,
        }
        let a = Cli::try_parse_from(["setup", "--unattended", "--server", "http://h:7070", "--token", "-Hq3_z", "--labels", "baremetal,bench", "--name", "box1", "--slots", "4"])?.a;
        assert_eq!((a.server.as_deref(), a.token.as_deref(), a.labels.len(), a.slots), (Some("http://h:7070"), Some("-Hq3_z"), 2, Some(4)));
        assert!(Cli::try_parse_from(["setup", "--domain", "kiln.home", "--no-front-door"]).is_err(), "one or the other");
        assert!(Cli::try_parse_from(["setup", "--dns", "resolver", "--on", "docker", "--sudo", "--no-tailnet", "--no-signing", "--no-open", "--dry-run", "-y"]).is_ok());
        assert_eq!(home_short_with("/home/ada/.local/bin/kiln", "/home/ada"), "~/.local/bin/kiln");
        assert_eq!(home_short_with("/home/adam/x", "/home/ada"), "/home/adam/x");
        assert!(preloaded("kiln.dev") && !preloaded("kiln.internal") && !preloaded("kiln.devices.home"));
        Ok(())
    }
}
