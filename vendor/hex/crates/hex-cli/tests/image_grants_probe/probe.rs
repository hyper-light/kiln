//! The grants probe (joint plan step 15, AC-T7.7.2; docs/design/network-grants.md, Tests): the one harness of
//! image_grants_real's probe image, whose program `scripts/grants-probe/build.sh` builds from `bin.rs` and this
//! file, std alone, static. It takes each TCP port it is told to coming in; on each connection there it tries
//! each egress target it is told of, once, and answers with one line for the port the connection came in on
//! and one for each target, its outcome typed ([`Outcome`]), then closes. Each line on its stdin asks the same of
//! it, answered on its stdout ([`answer`]): where its caller drives its harness (`ShardsImage::members`), a VM
//! granted nothing, so publishing no port, is asked so; elsewhere its stdin is the null device. It never ends on its
//! own: its VM's stop ends it. The test reads its answer with [`Report::parse`].
//!
//! Each try waits for its own TCP stack's answer, as a connect does: made, refused (a reset), no route, or its own
//! retransmissions run out (RFC 9293 §3.8.3, RFC 6298), never a wait of the probe's own. Arguments, its
//! harness.json's options, each explicit: `--address <ip>`, where it listens (in its VM, every address: `0.0.0.0`);
//! then any number of `--listen <port>` and `--egress <label>=<host>:<port>`.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufRead, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::thread::JoinHandle;

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
    /// Neither made nor refused before its TCP stack's own retransmissions ran out.
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

/// Where the probe was asked for its report: a connection to a TCP port it listens on, or a line on its stdin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ingress {
    Port(u16),
    Stdin,
}

/// What the probe answers one ask with: where it was asked, then each target's outcome, in the order it was told
/// them. Its text is a line `ingress <port>` or `ingress stdin`, then a line `egress <label> <host:port> <outcome>`
/// each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub ingress: Ingress,
    pub egress: Vec<(Target, Outcome)>,
}

impl Report {
    /// The report for `ingress` of each of `targets` tried once, in turn.
    fn of(ingress: Ingress, targets: &[Target]) -> Self {
        Self {
            ingress,
            egress: targets
                .iter()
                .map(|target| (target.clone(), reach(target)))
                .collect(),
        }
    }

    fn text(&self) -> String {
        let mut out = match self.ingress {
            Ingress::Port(port) => format!("ingress {port}\n"),
            Ingress::Stdin => "ingress stdin\n".to_owned(),
        };
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
        let ingress = match lines.next().and_then(|line| line.strip_prefix("ingress ")) {
            Some("stdin") => Ingress::Stdin,
            Some(port) => Ingress::Port(
                port.parse()
                    .map_err(|_| format!("ingress {port:?} is no port and not stdin"))?,
            ),
            None => return Err(format!("no ingress line first: {text:?}")),
        };
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

/// What the probe was told: where and on which ports it listens, its targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Told {
    pub address: IpAddr,
    pub listen: Vec<u16>,
    pub targets: Vec<Target>,
}

impl Told {
    /// Its arguments, read whole, or why they say nothing it can do: each flag with its value, an address and at
    /// least one port, every port 0 to 65535 (0 the OS's pick, for the probe's own tests), every label one word.
    pub fn read(args: &[OsString]) -> Result<Self, String> {
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
        let address = address.ok_or("no --address")?;
        if listen.is_empty() {
            return Err("no --listen port".to_owned());
        }
        Ok(Self {
            address,
            listen,
            targets,
        })
    }
}

/// Tries `target` once: its name resolved through the VM's own resolver, then each address it gave in turn, until
/// one connects, each connect waiting for its TCP stack's answer.
pub fn reach(target: &Target) -> Outcome {
    let Ok(addresses) = target.at.to_socket_addrs() else {
        return Outcome::Unresolved;
    };
    let mut outcome = Outcome::Unresolved;
    for address in addresses {
        match TcpStream::connect(address) {
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

/// Takes each port `told` names, at its address, and serves each on a thread of its own.
pub fn start(told: &Told) -> Result<Started, String> {
    let mut at = Vec::new();
    let mut servers = Vec::new();
    for port in &told.listen {
        let listener = TcpListener::bind((told.address, *port))
            .map_err(|e| format!("listening on {port}: {e}"))?;
        let bound = listener
            .local_addr()
            .map_err(|e| format!("the address listening on {port}: {e}"))?;
        let targets = told.targets.clone();
        let server = std::thread::Builder::new()
            .name(format!("probe-{}", bound.port()))
            .spawn(move || serve(&listener, bound.port(), &targets))
            .map_err(|e| format!("a thread for {port}: {e}"))?;
        at.push(bound);
        servers.push(server);
    }
    Ok(Started { at, servers })
}

/// Answers each connection `listener` takes with its report, one at a time; a connection that fails as it is taken
/// or answered is that connection's end alone.
fn serve(listener: &TcpListener, port: u16, targets: &[Target]) {
    for connection in listener.incoming() {
        let Ok(mut connection) = connection else {
            continue;
        };
        let report = Report::of(Ingress::Port(port), targets);
        let _ = connection.write_all(report.text().as_bytes());
    }
}

/// Answers each line `input` gives with its report on `output`, ended by an empty line and flushed, until `input`
/// ends: the probe's stdin and stdout, where its caller drives its harness.
pub fn answer(input: impl BufRead, mut output: impl Write, targets: &[Target]) -> io::Result<()> {
    for line in input.lines() {
        line?;
        let report = Report::of(Ingress::Stdin, targets);
        output.write_all(report.text().as_bytes())?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}
