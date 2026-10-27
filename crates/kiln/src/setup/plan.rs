//! What `kiln setup` does, decided from what it found on this machine ([`Facts`]) and what was
//! chosen ([`Answers`]: the person's answers, else the flags and the defaults). Pure, so each
//! combination is a test: the defaults are [`defaults`], the steps and what's said beside them
//! [`plan`].

use crate::frontdoor::{self, DnsMode, Settings};
use crate::install::On;

/// The front door's name unless another is chosen: under `.internal`, which ICANN keeps for
/// private networks, so it hides no site on the internet and no browser insists on HTTPS for it.
pub const DOMAIN: &str = "kiln.internal";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Os {
    #[default]
    Linux,
    Macos,
    Windows,
}

impl Os {
    pub fn this() -> Os {
        if cfg!(target_os = "macos") {
            Os::Macos
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Os::Linux => "Linux",
            Os::Macos => "macOS",
            Os::Windows => "Windows",
        }
    }
}

/// What setup found on this machine.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub os: Os,
    pub host: String,
    pub root: bool,
    /// kiln is installed here: its config file is (whether or not its server answers).
    pub installed: bool,
    /// The kiln server on this machine, when one answers.
    pub server: Option<Server>,
    /// The kiln server this machine is a runner of, when it's enrolled with one.
    pub runner_of: Option<String>,
    /// Docker answers for this user.
    pub docker: bool,
    /// The kubeconfig's current context.
    pub cluster: Option<String>,
    /// kiln servers on this network this machine could join.
    pub found: Vec<Found>,
    pub signing: Signing,
    /// Tailscale on this machine, when it's installed.
    pub tailscale: Option<Tailnet>,
    /// The lowest port this user may listen on (Linux's `ip_unprivileged_port_start`).
    pub low_port: u16,
    /// ufw filters this machine's traffic.
    pub ufw: bool,
    /// The LAN's subnets (`192.168.1.0/24`) and this machine's addresses on it.
    pub lans: Vec<String>,
    pub addresses: Vec<String>,
}

/// The kiln server on this machine, as it answered.
#[derive(Clone, Debug, Default)]
pub struct Server {
    pub url: String,
    /// Its own runner (`[local_agent]`), whose front door is this machine's.
    pub runner: Option<String>,
    /// That front door, as it's set.
    pub front_door: Option<Settings>,
    /// It has a Tailscale key, to keep the tailnet's DNS pointed at its front doors.
    pub tailnet_key: bool,
    /// It has a sign-in password.
    pub password: bool,
}

/// A kiln server heard on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub url: String,
    pub version: String,
}

/// kiln's own code signature on this machine (`crate::os::macsign`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Signing {
    /// kiln signs its builds here.
    Ready,
    /// A Mac without it yet.
    Missing,
    /// Only a Mac needs it.
    #[default]
    NotNeeded,
}

/// Tailscale on this machine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tailnet {
    /// Signed in and running, on a TUN interface (what a front door can listen on).
    pub usable: bool,
    /// Why it isn't, when it isn't.
    pub why: String,
    pub tailnet: String,
    /// This machine's MagicDNS name and tailnet address.
    pub name: String,
    pub address: Option<String>,
}

/// Where kiln goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A new kiln: on this machine, in Docker on it, or in Kubernetes.
    New(On),
    /// This machine as a runner of the kiln at `server`: with its join token, or asking to join.
    Join { server: String, token: bool },
    /// The kiln on this machine, as it is.
    Here,
    /// This machine is a runner of `0` already.
    Runner(String),
}

impl Target {
    /// A kiln server on this machine itself, whose runner's front door is this machine's.
    pub fn here(&self) -> bool {
        matches!(self, Target::New(On::Machine) | Target::Here)
    }
}

/// The front door to turn on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Door {
    pub domain: String,
    pub dns: DnsMode,
    /// On the tailnet too.
    pub tailnet: bool,
}

/// What setup was told to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answers {
    pub target: Target,
    /// Give kiln its own code signature (a Mac's).
    pub sign: bool,
    /// The front door to turn on; `None`: leave it as it is.
    pub door: Option<Door>,
    /// Take the one-time steps that need root (rather than leave them for the person).
    pub root_steps: bool,
    /// A Tailscale key was given, for the tailnet's DNS.
    pub tailnet_key: bool,
    /// A sign-in password was given.
    pub password: bool,
}

/// What setup starts from: the flags, and where it's asked nothing, what it does on its own.
/// Asking (`asking`), it offers more, and the person says yes or no to each: kiln's signature on
/// a Mac and the front door are offered; unattended, the front door comes only with `--domain`,
/// root's steps only with `--sudo`, and the signature not at all (macOS asks a person about it).
pub fn defaults(f: &Facts, a: &super::SetupArgs, asking: bool) -> Answers {
    let target = match (&a.server, a.on) {
        (Some(server), _) => Target::Join { server: server.clone(), token: a.token.is_some() },
        (None, Some(on)) => Target::New(on),
        (None, None) if f.server.is_some() => Target::Here,
        (None, None) if !f.installed && f.runner_of.is_some() => Target::Runner(f.runner_of.clone().unwrap_or_default()),
        (None, None) => Target::New(On::Machine),
    };
    let set = f.server.as_ref().and_then(|s| s.front_door.as_ref()).is_some_and(|d| d.dns.enabled || d.https.enabled);
    let door = if !target.here() || a.no_front_door {
        None
    } else if let Some(domain) = &a.domain {
        Some(door(f, a, domain))
    } else if asking && !set {
        Some(door(f, a, DOMAIN))
    } else {
        None
    };
    Answers { sign: asking && f.signing == Signing::Missing && !a.no_signing, root_steps: asking || a.sudo, door, target, tailnet_key: false, password: false }
}

/// The front door at `domain`, as setup turns it on unless told otherwise.
pub fn door(f: &Facts, a: &super::SetupArgs, domain: &str) -> Door {
    Door {
        domain: domain.trim().to_ascii_lowercase(),
        dns: a.dns.map_or(DnsMode::Authoritative, super::Dns::mode),
        tailnet: f.tailscale.as_ref().is_some_and(|t| t.usable) && !a.no_tailnet,
    }
}

/// The front door's settings for `d`: DNS for its name (port 53), HTTPS (443, with plain HTTP on
/// 80 sending browsers there) on kiln's own certificate, its first label's `.local` name, and the
/// tailnet when it's on.
pub fn settings(d: &Door) -> Settings {
    let label = d.domain.split('.').next().and_then(|l| frontdoor::local_label(l).ok()).unwrap_or_else(|| frontdoor::MDNS_NAME.to_owned());
    let mut s = Settings { domain: d.domain.clone(), ..Settings::default() };
    s.dns.enabled = true;
    s.dns.mode = d.dns;
    s.https.enabled = true;
    s.mdns.enabled = true;
    s.mdns.name = label;
    s.tailscale.enabled = d.tailnet;
    s
}

/// A command for root: through sudo, unless setup runs as root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    /// What it's given on its input.
    pub input: Option<String>,
}

impl Command {
    fn new(program: &str, args: &[&str]) -> Command {
        Command { program: program.to_owned(), args: args.iter().map(|a| (*a).to_owned()).collect(), input: None }
    }

    /// As a person would type it (`root`: setup runs as root, so without sudo).
    pub fn shown(&self, root: bool) -> String {
        let run = format!("{}{} {}", if root { "" } else { "sudo " }, self.program, self.args.join(" "));
        match &self.input {
            Some(i) => format!("echo '{}' | {run}", i.trim_end()),
            None => run,
        }
    }
}

/// The one-time root steps the front door `d` needs on this machine, and why: on Linux, ports
/// below 1024 (`crate::frontdoor::netinfo::privileged_ports_fix`, the same commands), and ufw's
/// rules for the LAN where ufw is on.
pub fn root_steps(f: &Facts, d: &Settings) -> Vec<(String, Vec<Command>)> {
    let mut out = Vec::new();
    if f.os != Os::Linux {
        return out;
    }
    let lowest = [d.dns.enabled.then_some(d.dns.port), d.https.enabled.then_some(d.https.port), (d.https.enabled && d.https.http_port > 0).then_some(d.https.http_port)]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(frontdoor::DNS_PORT);
    if f.low_port > lowest {
        let line = format!("net.ipv4.ip_unprivileged_port_start = {lowest}");
        out.push((
            format!("Let users listen on ports from {lowest} up"),
            vec![
                Command::new("sysctl", &["-w", &format!("net.ipv4.ip_unprivileged_port_start={lowest}")]),
                Command { input: Some(format!("{line}\n")), ..Command::new("tee", &["/etc/sysctl.d/99-kiln.conf"]) },
            ],
        ));
    }
    if f.ufw && !f.lans.is_empty() {
        let mut tcp: Vec<u16> = Vec::new();
        let mut udp: Vec<u16> = Vec::new();
        if d.dns.enabled {
            tcp.push(d.dns.port);
            udp.push(d.dns.port);
        }
        if d.https.enabled {
            tcp.push(d.https.port);
            if d.https.http_port > 0 {
                tcp.push(d.https.http_port);
            }
        }
        if d.mdns.enabled {
            udp.push(crate::discovery::MDNS_PORT);
        }
        tcp.sort_unstable();
        udp.sort_unstable();
        let list = |p: &[u16]| p.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
        let mut commands = Vec::new();
        for net in &f.lans {
            for (ports, proto) in [(&tcp, "tcp"), (&udp, "udp")] {
                if !ports.is_empty() {
                    commands.push(Command::new("ufw", &["allow", "from", net, "to", "any", "port", &list(ports), "proto", proto]));
                }
            }
        }
        out.push((format!("Let {} through ufw to the front door", f.lans.join(", ")), commands));
    }
    out
}

/// A step setup takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// kiln's own code signature on this Mac (`kiln signing setup`).
    Sign,
    /// Commands for root, said first.
    Root { why: String, commands: Vec<Command> },
    Install(On),
    Join { server: String, token: bool },
    /// The front door of this machine's kiln's runner.
    FrontDoor(Box<Settings>),
    /// Tailscale's key, so kiln keeps the tailnet's DNS pointed at the front door.
    TailnetKey,
    Password,
}

/// How a thing said stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Done,
    /// A person's to do, or not as it should be.
    Left,
    Info,
}

/// Something said beside the steps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub level: Level,
    pub text: String,
    pub detail: String,
}

fn note(level: Level, text: impl Into<String>, detail: impl Into<String>) -> Note {
    Note { level, text: text.into(), detail: detail.into() }
}

/// What setup will do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub notes: Vec<Note>,
}

/// The steps for `a` on `f`, in the order they're taken: first what a person is asked for at the
/// machine (macOS's question, sudo's password), so the rest can run without them; then kiln; then
/// what's set in it.
pub fn plan(f: &Facts, a: &Answers) -> Plan {
    let mut p = Plan::default();
    match f.signing {
        Signing::Ready => p.notes.push(note(Level::Done, "kiln signs its builds on this Mac", "macOS knows every upgrade as one program")),
        Signing::Missing if a.sign => p.steps.push(Step::Sign),
        Signing::Missing => p.notes.push(note(Level::Left, "Upgrades here wait for you", "kiln has no signature of its own on this Mac: `kiln signing setup` gives it one")),
        Signing::NotNeeded => p.notes.push(note(Level::Done, "Upgrades need nothing more here", "only a Mac needs kiln's own signature")),
    }
    let door = a.door.as_ref().map(settings);
    if let Some(d) = &door {
        for (why, commands) in root_steps(f, d) {
            if a.root_steps {
                p.steps.push(Step::Root { why, commands });
            } else {
                p.notes.push(note(Level::Left, why, commands.iter().map(|c| c.shown(f.root)).collect::<Vec<_>>().join("; ")));
            }
        }
    }
    match &a.target {
        Target::New(on) => p.steps.push(Step::Install(*on)),
        Target::Join { server, token } => p.steps.push(Step::Join { server: server.clone(), token: *token }),
        Target::Here => p.notes.push(note(Level::Done, "kiln runs on this machine", f.server.as_ref().map(|s| s.url.clone()).unwrap_or_default())),
        Target::Runner(server) => p.notes.push(note(Level::Done, "This machine is a runner", format!("of {server}"))),
    }
    let set = f.server.as_ref().and_then(|s| s.front_door.as_ref()).filter(|d| d.dns.enabled || d.https.enabled);
    match (door, set) {
        (Some(d), _) => {
            let tailnet = d.tailscale.enabled;
            p.steps.push(Step::FrontDoor(Box::new(d)));
            if tailnet && a.tailnet_key {
                p.steps.push(Step::TailnetKey);
            }
        }
        (None, Some(d)) if a.target == Target::Here => p.notes.push(note(Level::Done, format!("The front door is on: {}", d.domain), d.summary())),
        (None, _) if a.target.here() => p.notes.push(note(Level::Info, "The front door stays off", "`kiln setup` again, or Runners → Domain & HTTPS, turns it on")),
        (None, _) => p.notes.push(note(Level::Info, "Its front door is set in kiln", "Runners → Domain & HTTPS, for the runner that serves it")),
    }
    if a.password {
        p.steps.push(Step::Password);
    } else if f.server.as_ref().is_some_and(|s| s.password) && a.target == Target::Here {
        p.notes.push(note(Level::Done, "A sign-in password is set", "`kiln passwd` changes it"));
    }
    p
}

/// What a step is, as the plan lists it: what, and more about it.
pub fn describe(f: &Facts, step: &Step) -> (String, String) {
    match step {
        Step::Sign => ("kiln's own code signature".into(), "macOS asks you once to trust it".into()),
        Step::Root { why, commands } => (why.clone(), commands.first().map(|c| c.shown(f.root)).map(|c| if commands.len() > 1 { format!("{c} …") } else { c }).unwrap_or_default()),
        Step::Install(On::Machine) => ("kiln on this machine".into(), format!("{}, started at boot", super::home_short(&crate::install::bin_path(f.root).display().to_string()))),
        Step::Install(On::Docker) => ("kiln in Docker".into(), "container kiln, its data in volume kiln-data, on port 7070".into()),
        Step::Install(On::Kubernetes) => ("kiln in Kubernetes".into(), format!("context {}, namespace kiln", f.cluster.as_deref().unwrap_or("(current)"))),
        Step::Join { server, token } => {
            (format!("This machine, as a runner of {server}"), if *token { "with its join token".into() } else { "it asks to join, and you approve it in kiln".into() })
        }
        Step::FrontDoor(s) => (format!("The front door: {}", s.domain), door_summary(s)),
        Step::TailnetKey => ("The tailnet's DNS, pointed here".into(), "kiln keeps it so, with your OAuth client".into()),
        Step::Password => ("A sign-in password".into(), "for any device, your phone too".into()),
    }
}

/// What a front door serves, in a few words.
pub fn door_summary(s: &Settings) -> String {
    let mut parts = Vec::new();
    if s.dns.enabled {
        parts.push(
            match s.dns.mode {
                DnsMode::Authoritative => "DNS",
                DnsMode::Resolver => "DNS (every name)",
            }
            .to_owned(),
        );
    }
    if s.https.enabled {
        parts.push("HTTPS, kiln's certificate".to_owned());
    }
    parts.extend(s.local_name());
    if s.tailscale.enabled {
        parts.push("tailnet".to_owned());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        a: super::super::SetupArgs,
    }

    fn args(words: &[&str]) -> anyhow::Result<super::super::SetupArgs> {
        Ok(Cli::try_parse_from(std::iter::once("setup").chain(words.iter().copied()))?.a)
    }

    fn linux() -> Facts {
        Facts { host: "box".into(), low_port: 1024, lans: vec!["192.168.1.0/24".into()], addresses: vec!["192.168.1.77".into()], ..Facts::default() }
    }

    fn mac() -> Facts {
        Facts { os: Os::Macos, signing: Signing::Missing, low_port: 1024, ..linux() }
    }

    fn tailscale() -> Tailnet {
        Tailnet { usable: true, tailnet: "ada@example.com".into(), name: "box.tail1.ts.net".into(), address: Some("100.64.0.5".into()), why: String::new() }
    }

    fn kinds(p: &Plan) -> Vec<&'static str> {
        p.steps
            .iter()
            .map(|s| match s {
                Step::Sign => "sign",
                Step::Root { .. } => "root",
                Step::Install(_) => "install",
                Step::Join { .. } => "join",
                Step::FrontDoor(_) => "front door",
                Step::TailnetKey => "tailnet key",
                Step::Password => "password",
            })
            .collect()
    }

    #[test]
    fn a_new_kiln_goes_on_this_machine_unless_told_otherwise() -> anyhow::Result<()> {
        let f = Facts { cluster: Some("kind-kiln".into()), docker: true, ..linux() };
        // Whatever kubeconfig is here: this machine (`kiln install`'s default is the cluster).
        assert_eq!(defaults(&f, &args(&[])?, true).target, Target::New(On::Machine));
        assert_eq!(defaults(&f, &args(&["--on", "kubernetes"])?, true).target, Target::New(On::Kubernetes));
        assert_eq!(defaults(&f, &args(&["--server", "http://h:7070"])?, true).target, Target::Join { server: "http://h:7070".into(), token: false });
        assert_eq!(defaults(&f, &args(&["--server", "http://h:7070", "--token", "-Hq3"])?, false).target, Target::Join { server: "http://h:7070".into(), token: true });
        // A kiln that answers here is set up further; a runner of another is left a runner.
        let here = Facts { installed: true, server: Some(Server { url: "http://localhost:7070".into(), ..Server::default() }), ..f.clone() };
        assert_eq!(defaults(&here, &args(&[])?, true).target, Target::Here);
        let runner = Facts { runner_of: Some("http://h:7070".into()), ..f.clone() };
        assert_eq!(defaults(&runner, &args(&[])?, true).target, Target::Runner("http://h:7070".into()));
        // Installed, but not answering: installed again (its config and data stay).
        assert_eq!(defaults(&Facts { installed: true, ..f }, &args(&[])?, true).target, Target::New(On::Machine));
        Ok(())
    }

    #[test]
    fn asked_the_front_door_is_offered_unattended_it_comes_only_with_a_domain() -> anyhow::Result<()> {
        let f = Facts { tailscale: Some(tailscale()), ..linux() };
        let asked = defaults(&f, &args(&[])?, true);
        assert_eq!(asked.door, Some(Door { domain: DOMAIN.into(), dns: DnsMode::Authoritative, tailnet: true }));
        assert!(asked.root_steps, "asked for (sudo's password) at the machine");
        let quiet = defaults(&f, &args(&["--unattended"])?, false);
        assert_eq!((quiet.door, quiet.root_steps, quiet.sign), (None, false, false));
        let named = defaults(&f, &args(&["--domain", "Kiln.Home", "--dns", "resolver", "--no-tailnet"])?, false);
        assert_eq!(named.door, Some(Door { domain: "kiln.home".into(), dns: DnsMode::Resolver, tailnet: false }));
        assert!(!named.root_steps, "root's steps unattended only with --sudo");
        assert!(defaults(&f, &args(&["--domain", "kiln.home", "--sudo"])?, false).root_steps);
        assert_eq!(defaults(&f, &args(&["--no-front-door"])?, true).door, None);
        // Not on this machine: its runner's front door isn't this machine's to set.
        assert_eq!(defaults(&f, &args(&["--on", "docker", "--domain", "kiln.home"])?, true).door, None);
        assert_eq!(defaults(&f, &args(&["--server", "http://h:7070"])?, true).door, None);
        // Without Tailscale running here, not on the tailnet.
        let off = Facts { tailscale: Some(Tailnet { usable: false, why: "it's stopped".into(), ..tailscale() }), ..linux() };
        assert_eq!(defaults(&off, &args(&[])?, true).door.map(|d| d.tailnet), Some(false));
        Ok(())
    }

    #[test]
    fn the_front_door_is_dns_https_and_a_local_name() {
        let s = settings(&Door { domain: "kiln.internal".into(), dns: DnsMode::Authoritative, tailnet: true });
        assert!(s.validated().is_ok(), "{:?}", s.validated());
        assert_eq!((s.dns.enabled, s.dns.port, s.dns.mode), (true, 53, DnsMode::Authoritative));
        assert_eq!((s.https.enabled, s.https.port, s.https.http_port), (true, 443, 80));
        assert_eq!(s.cert.mode, crate::frontdoor::CertMode::Kiln);
        assert_eq!(s.local_name().as_deref(), Some("kiln.local"));
        assert!(s.tailscale.enabled);
        assert_eq!(door_summary(&s), "DNS · HTTPS, kiln's certificate · kiln.local · tailnet");
        assert_eq!(settings(&Door { domain: "ci.example.com".into(), dns: DnsMode::Resolver, tailnet: false }).local_name().as_deref(), Some("ci.local"));
    }

    #[test]
    fn root_steps_are_linux_ports_and_ufw_said_as_typed() {
        let s = settings(&Door { domain: DOMAIN.into(), dns: DnsMode::Authoritative, tailnet: false });
        let steps = root_steps(&Facts { ufw: true, ..linux() }, &s);
        let shown: Vec<Vec<String>> = steps.iter().map(|(_, c)| c.iter().map(|c| c.shown(false)).collect()).collect();
        assert_eq!(
            shown,
            [
                vec!["sudo sysctl -w net.ipv4.ip_unprivileged_port_start=53".to_owned(), "echo 'net.ipv4.ip_unprivileged_port_start = 53' | sudo tee /etc/sysctl.d/99-kiln.conf".to_owned()],
                vec!["sudo ufw allow from 192.168.1.0/24 to any port 53,80,443 proto tcp".to_owned(), "sudo ufw allow from 192.168.1.0/24 to any port 53,5353 proto udp".to_owned()],
            ]
        );
        // The same commands the front door's status gives for the ports.
        let fix = crate::frontdoor::netinfo::privileged_ports_fix(53);
        assert_eq!(shown.first(), Some(&fix.commands));
        // Ports a user may listen on already, no ufw, or not Linux: nothing for root.
        assert!(root_steps(&Facts { low_port: 0, ..linux() }, &s).is_empty());
        assert!(root_steps(&mac(), &s).is_empty());
        assert_eq!(root_steps(&Facts { low_port: 53, ufw: true, ..linux() }, &s).len(), 1);
        assert_eq!(steps.first().and_then(|(_, c)| c.first()).map(|c| c.shown(true)), Some("sysctl -w net.ipv4.ip_unprivileged_port_start=53".to_owned()), "as root, without sudo");
    }

    #[test]
    fn a_mac_is_signed_first_and_the_person_is_asked_for_everything_before_kiln_installs() -> anyhow::Result<()> {
        let f = Facts { ufw: true, ..mac() };
        let mut a = defaults(&f, &args(&[])?, true);
        a.password = true;
        let p = plan(&f, &a);
        assert_eq!(kinds(&p), ["sign", "install", "front door", "password"], "no root steps on a Mac");
        let mut a = defaults(&Facts { ufw: true, tailscale: Some(tailscale()), ..linux() }, &args(&[])?, true);
        a.tailnet_key = true;
        let p = plan(&Facts { ufw: true, tailscale: Some(tailscale()), ..linux() }, &a);
        assert_eq!(kinds(&p), ["root", "root", "install", "front door", "tailnet key"]);
        assert!(p.notes.iter().any(|n| n.level == Level::Done && n.text.contains("Upgrades need nothing more")));
        Ok(())
    }

    #[test]
    fn what_isnt_done_is_said() -> anyhow::Result<()> {
        // A Mac whose signature was declined: upgrades wait for the person, and that's said.
        let mut a = defaults(&mac(), &args(&["--no-signing"])?, true);
        a.root_steps = false;
        let p = plan(&mac(), &a);
        assert!(!kinds(&p).contains(&"sign"));
        assert!(p.notes.iter().any(|n| n.level == Level::Left && n.detail.contains("kiln signing setup")), "{:?}", p.notes);
        // Root's steps declined: left for the person, as commands.
        let mut a = defaults(&linux(), &args(&[])?, true);
        a.root_steps = false;
        let p = plan(&linux(), &a);
        assert_eq!(kinds(&p), ["install", "front door"]);
        assert!(p.notes.iter().any(|n| n.level == Level::Left && n.detail.contains("sudo sysctl")), "{:?}", p.notes);
        // A kiln here with its front door on: nothing more to do, and it says so.
        let on = settings(&Door { domain: "kiln.home".into(), dns: DnsMode::Authoritative, tailnet: false });
        let here = Facts { installed: true, server: Some(Server { url: "http://localhost:7070".into(), runner: Some("box".into()), front_door: Some(on), password: true, ..Server::default() }), ..linux() };
        let p = plan(&here, &defaults(&here, &args(&[])?, true));
        assert!(p.steps.is_empty(), "{:?}", p.steps);
        let said: Vec<&str> = p.notes.iter().map(|n| n.text.as_str()).collect();
        assert_eq!(said, ["Upgrades need nothing more here", "kiln runs on this machine", "The front door is on: kiln.home", "A sign-in password is set"]);
        // A runner joining elsewhere: its front door is the server's to set.
        let p = plan(&linux(), &defaults(&linux(), &args(&["--server", "http://h:7070", "--token", "t"])?, false));
        assert_eq!(kinds(&p), ["join"]);
        assert!(p.notes.iter().any(|n| n.text == "Its front door is set in kiln"));
        Ok(())
    }

    #[test]
    fn steps_are_described_for_the_plan() {
        let f = Facts { cluster: Some("kind-kiln".into()), ..linux() };
        assert_eq!(describe(&f, &Step::Install(On::Kubernetes)).1, "context kind-kiln, namespace kiln");
        assert_eq!(describe(&f, &Step::Join { server: "http://h:7070".into(), token: false }).0, "This machine, as a runner of http://h:7070");
        let root = root_steps(&Facts { ufw: true, ..linux() }, &settings(&Door { domain: DOMAIN.into(), dns: DnsMode::Authoritative, tailnet: false }));
        let (why, commands) = root.first().cloned().unwrap_or_default();
        assert_eq!(describe(&f, &Step::Root { why, commands }).1, "sudo sysctl -w net.ipv4.ip_unprivileged_port_start=53 …");
    }
}
