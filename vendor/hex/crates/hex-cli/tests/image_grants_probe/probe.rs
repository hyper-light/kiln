//! The grants probe (joint plan step 15, AC-T7.7.2; docs/design/network-grants.md, Tests): the one harness of
//! image_grants_real's probe image, whose program `scripts/grants-probe/build.sh` builds from `bin.rs` and this
//! file, std alone, static. It takes each TCP port it is told to coming in; on each connection there it tries
//! each egress target it is told of, once, and answers with one line for the port the connection came in on
//! and one for each target, its outcome typed ([`Outcome`]), then closes. It never ends on its own: its VM's
//! stop ends it. The test reads its answer with [`Report::parse`].
//!
//! Arguments, its harness.json's options, each explicit: `--wait-ms <n>`, how long a connection neither made nor
//! refused is waited for (the recipe's input, never a number of the probe's own); `--address <ip>`, where it
//! listens (in its VM, every address: `0.0.0.0`); then any number of `--listen <port>` and
//! `--egress <label>=<host>:<port>`.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::thread::JoinHandle;
use std::time::Duration;

/// How a try to reach a target ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The connection was made.
    Connected,
    /// It was refused or reset: by the peer, or by what stands between.
    Refused,
    /// The name resolved to no address: the resolver refused it, or knew none.
    Unresolved,
    /// No route to its network or host.
    Unreachable,
    /// Neither made nor refused within the probe's wait.
    TimedOut,
    /// Another failure, by its `io::ErrorKind` as Rust names it.
    Failed(String),
}

impl Outcome {
    fn of(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted => Self::Refused,
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Self::TimedOut,
            io::ErrorKind::NetworkUnreachable | io::ErrorKind::HostUnreachable => Self::Unreachable,
            kind => Self::Failed(format!("{kind:?}")),
        }
    }

    fn parse(word: &str) -> Result<Self, String> {
        Ok(match word {
            "connected" => Self::Connected,
            "refused" => Self::Refused,
            "unresolved" => Self::Unresolved,
            "unreachable" => Self::Unreachable,
            "timed-out" => Self::TimedOut,
            other => match other.strip_prefix("failed:") {
                Some(kind) if !kind.is_empty() => Self::Failed(kind.to_owned()),
                _ => return Err(format!("{word:?} is no outcome")),
            },
        })
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connected => f.write_str("connected"),
            Self::Refused => f.write_str("refused"),
            Self::Unresolved => f.write_str("unresolved"),
            Self::Unreachable => f.write_str("unreachable"),
            Self::TimedOut => f.write_str("timed-out"),
            Self::Failed(kind) => write!(f, "failed:{kind}"),
        }
    }
}

/// One egress target: its label, and the `host:port` tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub label: String,
    pub at: String,
}

/// What the probe answers one connection with: the port it came in on, then each target's outcome, in the order
/// it was told them. Its text is a line `ingress <port>`, then a line `egress <label> <host:port> <outcome>` each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub ingress: u16,
    pub egress: Vec<(Target, Outcome)>,
}

impl Report {
    fn text(&self) -> String {
        let mut out = format!("ingress {}\n", self.ingress);
        for (target, outcome) in &self.egress {
            out.push_str(&format!(
                "egress {} {} {outcome}\n",
                target.label, target.at
            ));
        }
        out
    }

    /// The report `text` says, or why it is none.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        let ingress = lines
            .next()
            .and_then(|line| line.strip_prefix("ingress "))
            .and_then(|port| port.parse().ok())
            .ok_or_else(|| format!("no ingress line first: {text:?}"))?;
        let mut egress = Vec::new();
        for line in lines {
            let words: Vec<&str> = line.split(' ').collect();
            let ["egress", label, at, outcome] = words.as_slice() else {
                return Err(format!("{line:?} is no egress line"));
            };
            let target = Target {
                label: (*label).to_owned(),
                at: (*at).to_owned(),
            };
            egress.push((target, Outcome::parse(outcome)?));
        }
        Ok(Self { ingress, egress })
    }
}

/// What the probe was told: how long it waits, where and on which ports it listens, its targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Told {
    pub wait: Duration,
    pub address: IpAddr,
    pub listen: Vec<u16>,
    pub targets: Vec<Target>,
}

impl Told {
    /// Its arguments, read whole, or why they say nothing it can do: each flag with its value, a wait, an address
    /// and at least one port, every port 0 to 65535 (0 the OS's pick, for the probe's own tests), every label one
    /// word.
    pub fn read(args: &[OsString]) -> Result<Self, String> {
        let mut wait = None;
        let mut address = None;
        let mut listen = Vec::new();
        let mut targets = Vec::new();
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            let value = rest
                .next()
                .and_then(|value| value.to_str())
                .ok_or_else(|| format!("{flag:?} has no value, or one that isn't UTF-8"))?;
            match flag.to_str() {
                Some("--wait-ms") => {
                    let ms: u64 = value
                        .parse()
                        .map_err(|_| format!("--wait-ms {value:?} is no count of milliseconds"))?;
                    wait = Some(Duration::from_millis(ms)).filter(|wait| !wait.is_zero());
                }
                Some("--address") => {
                    address = Some(
                        value
                            .parse()
                            .map_err(|_| format!("--address {value:?} is no IP address"))?,
                    );
                }
                Some("--listen") => listen.push(
                    value
                        .parse()
                        .map_err(|_| format!("--listen {value:?} is no TCP port"))?,
                ),
                Some("--egress") => {
                    let (label, at) = value
                        .split_once('=')
                        .filter(|(label, at)| {
                            !label.is_empty() && !label.contains(' ') && !at.is_empty()
                        })
                        .ok_or_else(|| format!("--egress {value:?} isn't <label>=<host>:<port>"))?;
                    targets.push(Target {
                        label: label.to_owned(),
                        at: at.to_owned(),
                    });
                }
                _ => return Err(format!("{flag:?} is no flag of the probe's")),
            }
        }
        let wait = wait.ok_or("no --wait-ms above 0")?;
        let address = address.ok_or("no --address")?;
        if listen.is_empty() {
            return Err("no --listen port".to_owned());
        }
        Ok(Self {
            wait,
            address,
            listen,
            targets,
        })
    }
}

/// Tries `target` once: its name resolved through the VM's own resolver, then each address it gave in turn, until
/// one connects.
pub fn reach(target: &Target, wait: Duration) -> Outcome {
    let Ok(addresses) = target.at.to_socket_addrs() else {
        return Outcome::Unresolved;
    };
    let mut outcome = Outcome::Unresolved;
    for address in addresses {
        match TcpStream::connect_timeout(&address, wait) {
            Ok(_) => return Outcome::Connected,
            Err(error) => outcome = Outcome::of(&error),
        }
    }
    outcome
}

/// The probe at work: the addresses it took, and its servers, one for each, which never end on their own.
pub struct Started {
    pub at: Vec<SocketAddr>,
    pub servers: Vec<JoinHandle<()>>,
}

/// Takes each port the arguments name, at their address, and serves each on a thread of its own.
pub fn start(args: &[OsString]) -> Result<Started, String> {
    let told = Told::read(args)?;
    let mut at = Vec::new();
    let mut servers = Vec::new();
    for port in &told.listen {
        let listener = TcpListener::bind((told.address, *port))
            .map_err(|e| format!("listening on {port}: {e}"))?;
        let bound = listener
            .local_addr()
            .map_err(|e| format!("the address listening on {port}: {e}"))?;
        let (wait, targets) = (told.wait, told.targets.clone());
        let server = std::thread::Builder::new()
            .name(format!("probe-{}", bound.port()))
            .spawn(move || serve(&listener, bound.port(), &targets, wait))
            .map_err(|e| format!("a thread for {port}: {e}"))?;
        at.push(bound);
        servers.push(server);
    }
    Ok(Started { at, servers })
}

/// Answers each connection `listener` takes with its report, one at a time; a connection that fails as it is taken
/// or answered is that connection's end alone.
fn serve(listener: &TcpListener, port: u16, targets: &[Target], wait: Duration) {
    for connection in listener.incoming() {
        let Ok(mut connection) = connection else {
            continue;
        };
        let report = Report {
            ingress: port,
            egress: targets
                .iter()
                .map(|target| (target.clone(), reach(target, wait)))
                .collect(),
        };
        let _ = connection.write_all(report.text().as_bytes());
    }
}
