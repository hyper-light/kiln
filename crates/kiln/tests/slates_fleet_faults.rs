//! kiln's Slates fleet under injected faults (docs/agentic.md W7, "The multi-runner test with injected faults"; kiln goal
//! 95101ef32ffb8b099942fe076619c310, its part 60178ceb; Slates' A-580). Three runners of a server of the test's own, each
//! its own machine (`KILN_MACHINE_ID`) with its own Slates instance (`KILN_SLATES_INSTANCE`), listed in the server's
//! `[slates_fleet]` with `f = 1` and nodes on loopback, form kiln's Slates fleet as W7's runners do. An agent of the
//! stand-in harness (no model) works in a captured clone on the first runner (W3, K5), and files are written into its
//! volume through Slates' own file verbs before each fault, each one Slates acknowledged. Each flow injects one fault,
//! with no privilege (a stopped process is the unprivileged partition, as in Slates' own `live_chaos.rs`), and every flow
//! ends with the three verdicts of `kiln::workspace::slates_faults`:
//!
//! - **no acknowledged change lost**: every acknowledged file is served after the fault byte for byte, read through any
//!   node, which serves it from whichever node holds the volume then;
//! - **the move completes or refuses by kind**: kiln's word on a move, once it settles, is the agent on its target, or a
//!   refusal naming a kind a move is refused by with the agent where it was;
//! - **the fleet re-forms**: once the fault ends, every listed runner's node serves as a member again.
//!
//! The link faults (A-708) put Slates' own fault relay (`vendor/slates/crates/cli/tests/support/relay.rs`, A-277)
//! between the nodes: each runner is listed at a block of ports the relay holds and binds its node's sockets elsewhere
//! (`[slates_fleet.binds]`), so every datagram between two nodes crosses the relay and meets its path's rule: a slow,
//! lossy link (a hold of `delay ± jitter`, so datagrams reorder, and a loss rate), or a runner cut off while its
//! processes run on. Every draw comes from one seed, printed with the relay's counts.
//!
//! It judges what holds, not how fast: no latency is judged. Each flow waits on the fleet's own failover delay as its
//! nodes report it (`daemon_status`), never a number of its own. Gated by `SLATES_FLEET_FAULTS=1`, since each flow starts
//! three Slates instances and their anchors; without it each skips, saying so. The flows that signal a daemon under its live
//! mounts (partitioned, restarted, a move's target cut) run on Linux alone, where a workspace is a FUSE mount: on macOS it's a
//! kernel NFS mount over loopback, and a daemon killed or stopped under a live NFS mount has panicked the Mac (Slates'
//! record of 2026-10-06). The link faults (A-708) signal no daemon, and the killed runner's flow lets every mount under the owner's
//! runner go before its kill (A-750), so they run on macOS too ([`Fault`]); there, as
//! everywhere, the lab's end stops its runners and unmounts every Slates mount under its folder before it ends an
//! instance, and leaves the instances running, saying so, where a mount won't go (as `slates_e2e`'s drives do).
//!
//!   SLATES_FLEET_FAULTS=1 cargo test -p kiln --test slates_fleet_faults -- --nocapture --test-threads 1

#![cfg(unix)]

mod common;
mod vm_e2e;

/// Slates' fault relay, as Slates' own chaos flows use it (A-277), included unchanged from the vendored tree (A-708).
#[allow(dead_code, clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]
#[path = "../../../vendor/slates/crates/cli/tests/support/relay.rs"]
mod relay;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use kiln::workspace::slates_faults::{self, MoveOutcome, MoveSaid};
use kiln::workspace::slates_fleet::Member;
use serde_json::{Value, json};

/// The switch that runs the flows; without it each skips, saying so.
const SWITCH: &str = "SLATES_FLEET_FAULTS";
/// The runners, by the names the test starts them as: three, the fewest `f = 1` takes (a write commits at f + 1 of
/// 2f + 1 copies).
const RUNNERS: [&str; 3] = ["fleet-a", "fleet-b", "fleet-c"];
/// The files written into the agent's volume before a fault, each acknowledged.
const FILES: usize = 8;
/// How long a flow may wait on the fleet at any one step, on a loaded machine: a start of three instances, a takeover,
/// the fleet re-forming. A step past it fails the flow saying what it waited for.
const WITHIN: Duration = Duration::from_secs(900);
/// How often a flow looks again while it waits.
const LOOK: Duration = Duration::from_millis(250);
/// A slow link (A-708): each datagram held this long, give or take [`SLOW_JITTER`], so datagrams reorder.
const SLOW_DELAY: Duration = Duration::from_millis(40);
/// The spread of a slow link's hold.
const SLOW_JITTER: Duration = Duration::from_millis(10);
/// A slow link's loss rate, in parts per million: 1 %.
const SLOW_LOSS_PPM: u32 = 10_000;

/// How a flow faults the fleet, which says where it may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// A signal to an instance's daemon (KILL, STOP): Linux alone, since one under a macOS NFS mount has panicked the Mac.
    Signal,
    /// A link through the relay (A-708): every daemon runs on, so any Unix.
    Link,
    /// A signal to a daemon whose runner has stopped and whose every mount has gone first (A-750): no NFS mount stands
    /// over it, so any Unix.
    Unmounted,
}

/// Why a flow of `fault` skips on this host (`linux`) with the switch `on` or off; none when it runs.
fn skip(fault: Fault, linux: bool, on: bool) -> Option<String> {
    if fault == Fault::Signal && !linux {
        return Some("skipped: a daemon is signalled on Linux alone (one stopped under a macOS workspace's NFS mount has panicked the Mac)".to_owned());
    }
    (!on).then(|| format!("skipped: {SWITCH}=1 runs kiln's Slates fleet under faults (three Slates instances and their anchors)"))
}

/// Whether a flow of `fault` runs here; a skip says why.
fn enabled(fault: Fault) -> bool {
    match skip(fault, cfg!(target_os = "linux"), std::env::var_os(SWITCH).is_some_and(|v| v == "1")) {
        Some(why) => {
            eprintln!("{why}");
            false
        }
        None => true,
    }
}

/// One runner of the lab: the name it was started as, its machine key (what `[slates_fleet.nodes]` lists), its Slates
/// instance, where its node is reached, and where its node binds when the relay stands between it and its peers.
struct Node {
    name: &'static str,
    machine: String,
    instance: String,
    address: String,
    bind: Option<SocketAddrV4>,
}

/// The lab: a server of the test's own and three runners forming kiln's Slates fleet; with the relay (A-708) between
/// their nodes, the daemon pid it attributes each node's sockets by.
struct Lab {
    fleet: common::Fleet,
    nodes: Vec<Node>,
    project: PathBuf,
    relay: Option<relay::Relay>,
    rooted: RefCell<Vec<Option<u32>>>,
}

/// Two consecutive UDP ports free on loopback: a node's base and the one after it (the base of its two UDP ports, W7).
fn two_ports() -> anyhow::Result<u16> {
    for _ in 0..64 {
        let base = common::free_udp_port()?;
        let next = base.checked_add(1).ok_or_else(|| anyhow::anyhow!("no port after {base}"))?;
        if std::net::UdpSocket::bind(("127.0.0.1", next)).is_ok() {
            return Ok(base);
        }
    }
    anyhow::bail!("no two consecutive UDP ports free on loopback")
}

/// Two consecutive UDP sockets the test holds on loopback: a relayed node's advertised block, the relay's to listen on.
fn advertised_pair() -> anyhow::Result<([UdpSocket; 2], u16)> {
    for _ in 0..64 {
        let first = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let base = first.local_addr()?.port();
        let Some(next) = base.checked_add(1) else { continue };
        if let Ok(second) = UdpSocket::bind((Ipv4Addr::LOCALHOST, next)) {
            return Ok(([first, second], base));
        }
    }
    anyhow::bail!("no two consecutive UDP ports free on loopback for a relayed node")
}

/// The relay's seed: `SLATES_CHAOS_SEED` replays a run's, as in Slates' own chaos flows; else one of this run's.
fn seed() -> u64 {
    std::env::var("SLATES_CHAOS_SEED")
        .ok()
        .and_then(|s| u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
        .unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() ^ u64::from(d.subsec_nanos())))
}

/// A nonce for this run's instance names.
fn nonce() -> String {
    format!("{}{}", std::process::id(), Instant::now().elapsed().subsec_nanos())
}

impl Lab {
    /// The server, its `[slates_fleet]` listing the three runners, and the runners started, each with its own instance
    /// and the stand-in under each harness's name; returns once every runner's node serves as a member.
    fn start() -> anyhow::Result<Lab> {
        let run = nonce();
        let mut nodes = Vec::new();
        for name in RUNNERS {
            let port = two_ports()?;
            nodes.push(Node { name, machine: common::machine(name), instance: format!("kilnflt{run}{name}").replace('-', ""), address: format!("127.0.0.1:{port}"), bind: None });
        }
        Lab::with_nodes(nodes, None)
    }

    /// [`Lab::start`] with Slates' fault relay between the nodes (A-708): each runner listed at a block the relay holds,
    /// its node bound at a block of its own (`[slates_fleet.binds]`), every path forwarding at once until a flow shapes it.
    fn start_relayed() -> anyhow::Result<Lab> {
        let run = nonce();
        let mut nodes = Vec::new();
        let mut relayed = Vec::new();
        for name in RUNNERS {
            let (advertised, at) = advertised_pair()?;
            let bound = SocketAddrV4::new(Ipv4Addr::LOCALHOST, two_ports()?);
            nodes.push(Node { name, machine: common::machine(name), instance: format!("kilnflt{run}{name}").replace('-', ""), address: format!("127.0.0.1:{at}"), bind: Some(bound) });
            relayed.push(relay::RelayedNode { advertised, bound });
        }
        let seed = seed();
        let relay = relay::Relay::start(relayed, seed);
        for (i, n) in nodes.iter().enumerate() {
            let base = n.bind.map_or(0, |b| b.port());
            for port in [Some(base), base.checked_add(1)].into_iter().flatten() {
                relay.claim(port, i);
            }
        }
        eprintln!("RELAY: seed {seed:#x} (SLATES_CHAOS_SEED replays it)");
        Lab::with_nodes(nodes, Some(relay))
    }

    /// The server with `[slates_fleet]` listing `nodes` (and their binds), the project, and every runner started; returns
    /// once every runner's node serves as a member.
    fn with_nodes(nodes: Vec<Node>, relay: Option<relay::Relay>) -> anyhow::Result<Lab> {
        let listed: String = nodes.iter().map(|n| format!("\"{}\" = \"{}\"\n", n.machine, n.address)).collect();
        let binds: String = nodes.iter().filter_map(|n| n.bind.map(|b| format!("\"{}\" = \"{b}\"\n", n.machine))).collect();
        let binds = if binds.is_empty() { binds } else { format!("[slates_fleet.binds]\n{binds}") };
        let extra = format!("[slates_fleet]\nf = 1\n[slates_fleet.nodes]\n{listed}{binds}");
        // `Fleet::with` starts the server itself.
        let fleet = common::Fleet::with(move |_| extra.clone())?;
        let project = fleet.root.path().join("project");
        make_project(&project)?;
        let rooted = RefCell::new(vec![None; nodes.len()]);
        let mut lab = Lab { fleet, nodes, project, relay, rooted };
        for i in 0..lab.nodes.len() {
            lab.start_runner(i)?;
        }
        lab.until_reformed("the fleet's first forming")?;
        // No operator step: kiln bootstraps its fleet once, by the runner its server names (A-734).
        lab.until_one_fleet()?;
        lab.on_hosts()?;
        Ok(lab)
    }

    /// Every runner's engine set to its host, and waited for until kiln's facts of it say so (as `vm_e2e::set_engine`
    /// does): the flows' agent works as a process on its runner in the runner's own workspace volume, which a move takes
    /// (W7); an isolated agent's microVM is its machine's, and kiln refuses to move it.
    fn on_hosts(&self) -> anyhow::Result<()> {
        for n in &self.nodes {
            let began = Instant::now();
            loop {
                let (code, body) = self.fleet.request("PUT", &format!("/api/runners/{}/engine", n.machine), Some(&json!({"engine": "host"})))?;
                if code == 200 {
                    break;
                }
                anyhow::ensure!(began.elapsed() < WITHIN, "{}'s engine set to host: {code} {}", n.name, String::from_utf8_lossy(&body));
                std::thread::sleep(LOOK);
            }
            loop {
                let engine = self.fleet.call("GET", &format!("/api/runners/{}/engine", n.machine), None)?;
                if engine.pointer("/isolation/engine").and_then(Value::as_str) == Some("host") {
                    break;
                }
                anyhow::ensure!(began.elapsed() < WITHIN, "{} never held engine host: {engine}", n.name);
                std::thread::sleep(LOOK);
            }
        }
        eprintln!("PROVEN: every runner's engine is its host");
        Ok(())
    }

    /// Waits until every node's daemon counts every node of the lab among its fleet's members: one fleet of three, where
    /// each node serving as a member of its own (`until_reformed`) was also three fleets of one (A-734).
    fn until_one_fleet(&self) -> anyhow::Result<()> {
        let began = Instant::now();
        loop {
            let counts: Vec<usize> = self.nodes.iter().map(|n| slates_api::Instance::attach(&n.instance).and_then(|mut i| i.daemon_status()).map_or(0, |r| r.fleet.members.len())).collect();
            if counts.iter().all(|c| *c == self.nodes.len()) {
                eprintln!("PROVEN: one fleet: every node counts all {} members ({:?})", self.nodes.len(), began.elapsed());
                return Ok(());
            }
            anyhow::ensure!(began.elapsed() < WITHIN, "the nodes never formed one fleet: members counted {counts:?}\n{}", self.fleet_seen());
            std::thread::sleep(LOOK);
        }
    }

    fn node(&self, i: usize) -> anyhow::Result<&Node> {
        self.nodes.get(i).ok_or_else(|| anyhow::anyhow!("no runner {i}"))
    }

    /// Runner `i` started, with its instance's environment and the stand-in.
    fn start_runner(&mut self, i: usize) -> anyhow::Result<()> {
        let (name, instance) = {
            let n = self.node(i)?;
            (n.name, n.instance.clone())
        };
        let dir = self.fleet.root.path().join(format!("{name}-stub"));
        let mut env = vm_e2e::stub_host_env(&dir)?;
        // Its own machine id (what `common::machine` keys it by): three runners on one host are three machines.
        env.extend([("KILN_MACHINE_ID", name.to_owned()), ("KILN_SLATES_INSTANCE", instance), ("KILN_SLATES_DIRECT", "1".to_owned()), ("KILN_SLATES", String::new())]);
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.fleet.start_runner_harnessed(name, &env)
    }

    /// Waits until every listed runner's node serves as a member ([`slates_faults::not_reformed`]), saying `what`.
    fn until_reformed(&self, what: &str) -> anyhow::Result<()> {
        let listed: Vec<&str> = self.nodes.iter().map(|n| n.machine.as_str()).collect();
        let began = Instant::now();
        loop {
            self.reroot();
            let members = self.members();
            let missing = slates_faults::not_reformed(&listed, &members);
            if missing.is_empty() {
                eprintln!("PROVEN: {what}: every listed runner's node serves as a member ({:?})", began.elapsed());
                return Ok(());
            }
            anyhow::ensure!(began.elapsed() < WITHIN, "{what}: the fleet didn't re-form within {WITHIN:?}; not serving: {missing:?}\n{}\n{}", self.fleet_seen(), self.slates_story());
            std::thread::sleep(LOOK);
        }
    }

    /// The relay told each node's daemon, so the record links it dials from fresh sockets are attributed to its node
    /// (the relay drops what no census attributes); again whenever a node's daemon is another process than last told.
    fn reroot(&self) {
        let Some(relay) = &self.relay else { return };
        let mut rooted = self.rooted.borrow_mut();
        for (i, n) in self.nodes.iter().enumerate() {
            let Ok(pid) = slates_api::Instance::attach(&n.instance).and_then(|mut d| d.daemon_status()).map(|r| r.pid) else { continue };
            if let Some(told) = rooted.get_mut(i).filter(|told| **told != Some(pid)) {
                relay.root(i, pid);
                *told = Some(pid);
            }
        }
    }

    /// The relay between the nodes, for a flow that shapes its paths.
    fn relay(&self) -> anyhow::Result<&relay::Relay> {
        self.relay.as_ref().ok_or_else(|| anyhow::anyhow!("this lab has no relay between its nodes"))
    }

    /// Fails unless the record plane's datagrams crossed the relay at all (A-330's check): a flow whose record links went
    /// around the relay would prove nothing of a link fault.
    fn crossed_the_relay(&self) -> anyhow::Result<()> {
        let report = self.relay()?.report();
        anyhow::ensure!(report.records_forwarded() > 0, "no record-plane datagram crossed the relay: {}\n{}", report.summary(), self.fleet_seen());
        eprintln!("RELAY: {}", report.summary());
        Ok(())
    }

    /// What each node's daemon says of its fleet (its host, the members it knows, the peers it probed, what it refused)
    /// and every manifest kiln wrote under the lab's folder: what a failure that's about the fleet's links shows.
    fn fleet_seen(&self) -> String {
        let mut out = String::new();
        for n in &self.nodes {
            let seen = slates_api::Instance::attach(&n.instance).and_then(|mut i| i.daemon_status()).map(|r| {
                let f = &r.fleet;
                format!("host {} f {} members {:?} peers_probed {} unknown_id {} sessions_refused {} configuration {} held_records {} takeovers_pending {}", f.host, f.f, f.members, f.peers_probed, f.unknown_id, f.sessions_refused, f.configuration_version, f.held_records, f.takeovers_pending)
            });
            out.push_str(&format!("--- {} at {} (bind {:?}): {}\n", n.name, n.address, n.bind, seen.unwrap_or_else(|e| format!("no status: {e}"))));
        }
        let mut dirs = vec![self.fleet.root.path().to_owned()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    dirs.push(path);
                } else if path.file_name().is_some_and(|f| f == kiln::workspace::slates_fleet::MANIFEST) {
                    out.push_str(&format!("--- {}\n{}\n", path.display(), std::fs::read_to_string(&path).unwrap_or_default()));
                }
            }
        }
        out
    }

    /// Every line of each runner's whole log that speaks of Slates or kiln's Slates fleet: what a formation failure shows
    /// (the fleet's own tail is the last lines alone, by then each runner's periodic word).
    fn slates_story(&self) -> String {
        let mut out = String::new();
        for n in &self.nodes {
            let log = self.fleet.root.path().join(format!("{}.log", n.name));
            out.push_str(&format!("--- {} (Slates and the fleet, whole log)\n", n.name));
            for line in std::fs::read_to_string(&log).unwrap_or_default().lines().filter(|l| l.contains("Slates") || l.contains("slates") || l.contains("fleet") || l.contains("move") || l.contains("moving")) {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// Each runner's node as a member, by its own instance's word: its member id where it serves as one.
    fn members(&self) -> Vec<Member> {
        self.nodes
            .iter()
            .map(|n| Member {
                machine: n.machine.clone(),
                address: n.address.clone(),
                domain: 0,
                certificate: String::new(),
                member: slates_api::Instance::attach(&n.instance).and_then(|mut i| i.member()).ok(),
                user: None,
            })
            .collect()
    }

    /// The fleet's failover delay as runner `i`'s node reports it now (A-363): what a fault is held past.
    fn failover(&self, i: usize) -> anyhow::Result<Duration> {
        let n = self.node(i)?;
        let report = slates_api::Instance::attach(&n.instance).and_then(|mut i| i.daemon_status()).map_err(|e| anyhow::anyhow!("{}: {e}", n.name))?;
        Ok(Duration::from_nanos(report.fleet.failover.total_ns))
    }

    /// An agent of the stand-in started on runner `i` in a workspace of its own: its session and its volume.
    fn start_agent(&self, i: usize) -> anyhow::Result<(String, slates_api::VolumeId)> {
        let on = self.node(i)?.machine.clone();
        let spec = json!({"kind": "claude", "on": on, "cwd": self.project, "prompt": "wait", "workspace": "volume", "isolation": "none"});
        // Asked again while kiln says the runner hasn't yet said what it has ("try again"): a runner's facts come a
        // moment after it connects, and a start before them is refused, not failed.
        let began = Instant::now();
        let started = loop {
            let (code, body) = self.fleet.request("POST", "/api/ops/agents.start", Some(&spec))?;
            let text = String::from_utf8_lossy(&body).into_owned();
            if (200..300).contains(&code) {
                break serde_json::from_str::<Value>(&text)?;
            }
            anyhow::ensure!(code == 409 && text.contains("try again") && began.elapsed() < WITHIN, "POST /api/ops/agents.start: {code} {text}");
            std::thread::sleep(LOOK);
        };
        // `agents.start` answers `{"agents": [brief], "parent"}` (harness/ops.rs `start_ask`).
        let id = ["/agents/0/id"]
            .iter()
            .find_map(|p| started.pointer(p).and_then(Value::as_str))
            .ok_or_else(|| anyhow::anyhow!("kiln named no session for the agent it started: {started}"))?
            .to_owned();
        let began = Instant::now();
        loop {
            let info = self.agent(&id)?;
            if let Some(volume) = info.pointer("/info/workspace/made/volume").and_then(Value::as_str).filter(|v| !v.trim().is_empty()) {
                return Ok((id, volume_id(volume)?));
            }
            anyhow::ensure!(began.elapsed() < WITHIN, "agent {id}'s workspace was never made: {info}");
            std::thread::sleep(LOOK);
        }
    }

    /// The refusal of a move kiln took for agent `id`, as its timeline says it (`harness::session::move_failed`'s notice,
    /// "Couldn't move to …"): the newest, or none.
    fn move_refused_in_timeline(&self, id: &str) -> anyhow::Result<Option<String>> {
        let events = self.fleet.call("GET", &format!("/api/harness/sessions/{id}/events?limit=1000"), None)?;
        fn said(v: &Value, out: &mut Option<String>) {
            match v {
                Value::String(t) if t.starts_with("Couldn't move to ") => *out = Some(t.clone()),
                Value::Array(a) => a.iter().for_each(|x| said(x, out)),
                Value::Object(o) => o.values().for_each(|x| said(x, out)),
                _ => {}
            }
        }
        let mut out = None;
        said(&events, &mut out);
        Ok(out)
    }

    /// kiln's word on agent `id` (`agents.get`).
    fn agent(&self, id: &str) -> anyhow::Result<Value> {
        self.fleet.call("POST", "/api/ops/agents.get", Some(&json!({"agent": id})))
    }

    /// Where agent `id` works now, by kiln's word: its runner's machine key.
    fn runs_on(&self, id: &str) -> anyhow::Result<Option<String>> {
        let info = self.agent(id)?;
        // `agents.get` answers `{"agent": brief, "info", ...}`; the brief's `machine` is its target's machine key.
        Ok(info.pointer("/agent/machine").and_then(Value::as_str).map(str::to_owned))
    }

    /// Every process of runner `i`'s instance (its anchor and its daemon): those whose command line names it.
    fn instance_pids(&self, i: usize) -> anyhow::Result<Vec<u32>> {
        let name = self.node(i)?.instance.clone();
        Ok(vm_e2e::host_processes()?.into_iter().filter(|p| names_instance(&p.argv, &name)).map(|p| p.pid).collect())
    }

    /// Sends `signal` to every process of runner `i`'s instance, saying what it did.
    fn signal(&self, i: usize, signal: &str) -> anyhow::Result<Vec<u32>> {
        let pids = self.instance_pids(i)?;
        anyhow::ensure!(!pids.is_empty(), "runner {i}'s instance has no process to send {signal}");
        for pid in &pids {
            let status = Command::new("kill").args([&format!("-{signal}"), &pid.to_string()]).status()?;
            anyhow::ensure!(status.success(), "kill -{signal} {pid}: {status}");
        }
        eprintln!("FAULT: {signal} to {} (pids {pids:?})", self.node(i)?.name);
        Ok(pids)
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        // The runners stopped and every Slates mount under the lab's folder unmounted first: no instance is ended under a
        // live mount (on macOS a kernel NFS mount, whose server's death has panicked the Mac). One that won't go leaves
        // every instance running, saying so.
        for n in RUNNERS {
            self.fleet.stop_runner(n);
        }
        let root = std::fs::canonicalize(self.fleet.root.path()).unwrap_or_else(|_| self.fleet.root.path().to_owned());
        let left = unmount_under(&root);
        if !left.is_empty() {
            eprintln!("LEFT RUNNING: the lab's Slates instances, under these mounts that wouldn't go: {left:?}");
            return;
        }
        // A stopped instance is let go on first, so its end is heard; then every instance the lab started is ended.
        for i in 0..self.nodes.len() {
            let _ = self.signal(i, "CONT");
            let _ = self.signal(i, "TERM");
        }
    }
}

/// The Slates mounts a host's table lists under `dir`, by their mount points: Linux's `/proc/self/mountinfo` (its fifth
/// field, and after ` - ` its type `fuse.slates` or a source `slates:…`), or BSD `mount`'s `<source> on <point> (<type>, …)`.
fn slates_mounts_under(table: &str, linux: bool, dir: &Path) -> Vec<String> {
    table
        .lines()
        .filter_map(|line| {
            if linux {
                let (head, tail) = line.split_once(" - ")?;
                let at = head.split(' ').nth(4)?.replace("\\040", " ");
                let mut rest = tail.split(' ');
                let (kind, source) = (rest.next()?, rest.next()?);
                (kind == "fuse.slates" || source.starts_with("slates:")).then_some(at)
            } else {
                let (source, rest) = line.split_once(" on ")?;
                let (at, _) = rest.rsplit_once(" (")?;
                source.starts_with("slates:").then(|| at.to_owned())
            }
        })
        .filter(|at| Path::new(at).starts_with(dir))
        .collect()
}

/// This host's Slates mounts under `dir` now; where the table can't be read, a word standing for one, so nothing is ended
/// unseen.
fn slates_mounts_now(dir: &Path) -> Vec<String> {
    let linux = cfg!(target_os = "linux");
    let table = if linux { std::fs::read_to_string("/proc/self/mountinfo").ok() } else { Command::new("mount").output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned()) };
    table.map_or_else(|| vec!["(the mount table couldn't be read)".to_owned()], |t| slates_mounts_under(&t, linux, dir))
}

/// Every Slates mount under `dir` unmounted (`umount`, then forced, on macOS; `fusermount3 -u`, then lazily, on Linux):
/// those still there after.
fn unmount_under(dir: &Path) -> Vec<String> {
    let tries: &[&[&str]] = if cfg!(target_os = "linux") { &[&["fusermount3", "-u"], &["fusermount3", "-u", "-z"]] } else { &[&["umount"], &["umount", "-f"]] };
    for at in slates_mounts_now(dir) {
        for t in tries {
            let Some((program, args)) = t.split_first() else { continue };
            if Command::new(program).args(args.iter()).arg(&at).output().is_ok_and(|o| o.status.success()) {
                break;
            }
        }
    }
    slates_mounts_now(dir)
}

/// Whether a command line names instance `name` (`kiln slates-role --instance <name> ...`).
fn names_instance(argv: &[String], name: &str) -> bool {
    argv.windows(2).any(|w| matches!(w, [flag, value] if flag == "--instance" && value == name)) || argv.iter().any(|a| a == &format!("--instance={name}"))
}

/// A Slates volume id from kiln's hex word for it.
fn volume_id(hex: &str) -> anyhow::Result<slates_api::VolumeId> {
    Ok(slates_api::VolumeId { bytes: u128::from_str_radix(hex.trim(), 16)?.to_be_bytes() })
}

/// A git project of a few committed files at `dir`: the folder the agent's workspace is captured from.
fn make_project(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(dir.join("README.md"), "the project\n")?;
    std::fs::write(dir.join("src").join("lib.rs"), "pub fn f() {}\n")?;
    for args in [vec!["init", "-q", "-b", "main"], vec!["add", "."], vec!["-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "commit", "-qm", "start"]] {
        let out = Command::new("git").args(&args).current_dir(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output()?;
        anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// [`FILES`] files written into `volume` through runner `i`'s instance, each kept once Slates acknowledged it: what the
/// verdict holds the fleet to.
fn write_acknowledged(lab: &Lab, i: usize, volume: slates_api::VolumeId, round: &str) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let n = lab.node(i)?;
    let mut acknowledged = BTreeMap::new();
    let mut instance = slates_api::Instance::attach(&n.instance).map_err(|e| anyhow::anyhow!("{}: {e}", n.name))?;
    let attachment = instance.attach_volume(volume, slates_api::Intent::Write).map_err(|e| anyhow::anyhow!("attach: {e}"))?.attachment;
    // At the volume's root: a file under a folder the volume lacks is refused, whatever the fleet does.
    let mut refused = None;
    for k in 0..FILES {
        let path = format!("/faults-{round}-{k}.txt");
        let bytes = format!("{round} {k}: acknowledged before the fault\n").into_bytes();
        match instance.client().fs_write((volume, attachment), &path, &bytes, 0o644) {
            Ok(_) => {
                acknowledged.insert(path, bytes);
            }
            Err(e) => {
                refused.get_or_insert_with(|| format!("{path}: {e:?}"));
            }
        }
    }
    let _ = instance.detach(attachment);
    anyhow::ensure!(acknowledged.len() == FILES, "only {} of {FILES} writes were acknowledged before the fault; the first refused: {}", acknowledged.len(), refused.unwrap_or_default());
    Ok(acknowledged)
}

/// Every acknowledged file read back through each node of `through` in turn, until one serves the volume: what is
/// served now.
fn served(lab: &Lab, through: &[usize], volume: slates_api::VolumeId, paths: &BTreeMap<String, Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    let mut found = BTreeMap::new();
    for path in paths.keys() {
        for n in through.iter().filter_map(|i| lab.nodes.get(*i)) {
            if let Ok(bytes) = slates_api::Instance::attach(&n.instance).and_then(|mut i| i.read(volume, path, slates_api::ReadAt::Head)) {
                found.insert(path.clone(), bytes);
                break;
            }
        }
    }
    found
}

/// Waits until no acknowledged file is lost ([`slates_faults::lost`]), which a takeover may take the failover delay to
/// give, and fails saying which are lost once [`WITHIN`] has passed.
fn no_acknowledged_change_lost(lab: &Lab, volume: slates_api::VolumeId, acknowledged: &BTreeMap<String, Vec<u8>>, what: &str) -> anyhow::Result<()> {
    let every: Vec<usize> = (0..lab.nodes.len()).collect();
    no_acknowledged_change_lost_through(lab, &every, volume, acknowledged, what)
}

/// [`no_acknowledged_change_lost`], read through the nodes of `through` alone: a runner cut off still answers from what
/// it held, so the fleet's keeping is read through the others.
fn no_acknowledged_change_lost_through(lab: &Lab, through: &[usize], volume: slates_api::VolumeId, acknowledged: &BTreeMap<String, Vec<u8>>, what: &str) -> anyhow::Result<()> {
    let began = Instant::now();
    loop {
        lab.reroot();
        let lost = slates_faults::lost(acknowledged, &served(lab, through, volume, acknowledged));
        if lost.is_empty() {
            eprintln!("PROVEN: {what}: every one of {} acknowledged files served byte for byte ({:?})", acknowledged.len(), began.elapsed());
            return Ok(());
        }
        // A read every node denies (rights, not loss) is no wait's to end: Slates' access lists are per host (A-237).
        let first = lost.first().map(String::as_str).unwrap_or_default();
        let denied = through.iter().filter_map(|i| lab.nodes.get(*i)).all(|n| matches!(slates_api::Instance::attach(&n.instance).and_then(|mut i| i.read(volume, first, slates_api::ReadAt::Head)), Err(slates_api::Error::Denied(_))));
        anyhow::ensure!(!denied, "{what}: every node of {through:?} denies the read of {first} (Slates' access lists are per host, A-237, and kiln has no library call to share a volume with another host's account)\n{}", read_refusals(lab, through, volume, first));
        anyhow::ensure!(began.elapsed() < WITHIN, "{what}: acknowledged changes lost: {lost:?}\n{}\n{}\n{}", read_refusals(lab, through, volume, lost.first().map(String::as_str).unwrap_or_default()), lab.fleet_seen(), lab.fleet.logs());
        std::thread::sleep(LOOK);
    }
}

/// What each node of `through` answers a read of `path` in `volume` now: what a lost file's failure says.
fn read_refusals(lab: &Lab, through: &[usize], volume: slates_api::VolumeId, path: &str) -> String {
    through
        .iter()
        .filter_map(|i| lab.nodes.get(*i))
        .map(|n| match slates_api::Instance::attach(&n.instance).and_then(|mut i| i.read(volume, path, slates_api::ReadAt::Head)) {
            Ok(bytes) => format!("--- {} reads {path}: {} bytes\n", n.name, bytes.len()),
            Err(e) => format!("--- {} reads {path}: {e:?}\n", n.name),
        })
        .collect()
}

/// W7, A-580, A-750: the owner's runner killed. Do: write acknowledged files into the agent's workspace on the first
/// runner, which kiln made live-shipped and shared with every other member (A-750, nothing of the lab's); stop its
/// runner, let every mount under it go, and kill its instance's anchor and daemon at once (the machine's loss); read the
/// files through the other two alone; start the runner again. Expect every acknowledged file served byte for byte by
/// the survivors, and the fleet re-formed once it's back. With its mounts gone first no NFS mount stands over the killed
/// daemon, so it runs on macOS too.
#[test]
fn w7_a_killed_runner_loses_no_acknowledged_change_and_the_fleet_reforms() -> anyhow::Result<()> {
    if !enabled(Fault::Unmounted) {
        return Ok(());
    }
    let mut lab = Lab::start()?;
    let (_agent, volume) = lab.start_agent(0)?;
    let acknowledged = write_acknowledged(&lab, 0, volume, "killed")?;
    no_acknowledged_change_lost_through(&lab, &[1, 2], volume, &acknowledged, "read through the other runners before the kill")?;
    let name = lab.node(0)?.name;
    lab.fleet.stop_runner(name);
    let root = std::fs::canonicalize(lab.fleet.root.path().join(name)).unwrap_or_else(|_| lab.fleet.root.path().join(name));
    let left = unmount_under(&root);
    anyhow::ensure!(left.is_empty(), "{name}'s mounts wouldn't go, so its daemon isn't killed beneath them: {left:?}");
    lab.signal(0, "KILL")?;
    no_acknowledged_change_lost_through(&lab, &[1, 2], volume, &acknowledged, "the owner's runner killed")?;
    lab.start_runner(0)?;
    lab.until_reformed("after the owner's runner killed")
}

/// W7, A-580: a runner partitioned. Do: write acknowledged files on the first runner; stop its instance's anchor and
/// daemon past the fleet's failover delay as its nodes report it; let them go on. Expect every acknowledged file served
/// byte for byte throughout what follows, and the fleet re-formed.
#[test]
fn w7_a_partitioned_runner_loses_no_acknowledged_change_and_the_fleet_reforms() -> anyhow::Result<()> {
    if !enabled(Fault::Signal) {
        return Ok(());
    }
    let lab = Lab::start()?;
    let (_agent, volume) = lab.start_agent(0)?;
    let acknowledged = write_acknowledged(&lab, 0, volume, "partitioned")?;
    let held = lab.failover(1)?;
    lab.signal(0, "STOP")?;
    std::thread::sleep(held.saturating_add(held));
    no_acknowledged_change_lost(&lab, volume, &acknowledged, "a runner partitioned")?;
    lab.signal(0, "CONT")?;
    no_acknowledged_change_lost(&lab, volume, &acknowledged, "a partitioned runner back")?;
    lab.until_reformed("after a runner partitioned")
}

/// W7, A-580: a runner restarted. Do: write acknowledged files on the first runner; stop its runner, end its daemon so
/// its anchor starts another (the volume in the anchor's memory), start its runner again. Expect every acknowledged file
/// served byte for byte, and the fleet re-formed with the runner back as the same machine.
#[test]
fn w7_a_restarted_runner_rejoins_and_loses_no_acknowledged_change() -> anyhow::Result<()> {
    if !enabled(Fault::Signal) {
        return Ok(());
    }
    let mut lab = Lab::start()?;
    let (_agent, volume) = lab.start_agent(0)?;
    let acknowledged = write_acknowledged(&lab, 0, volume, "restarted")?;
    let name = lab.node(0)?.name;
    lab.fleet.stop_runner(name);
    let daemon = slates_api::Instance::attach(&lab.node(0)?.instance).and_then(|mut i| i.daemon_status()).map_err(|e| anyhow::anyhow!("{e}"))?.pid;
    let status = Command::new("kill").args(["-KILL", &daemon.to_string()]).status()?;
    anyhow::ensure!(status.success(), "kill -KILL {daemon}: {status}");
    lab.start_runner(0)?;
    lab.until_reformed("after a runner restarted")?;
    no_acknowledged_change_lost(&lab, volume, &acknowledged, "a runner restarted")
}

/// W7, A-580: a link lost mid-move. Do: write acknowledged files on the first runner; ask kiln to move the agent to the
/// second (`agents.move`), and stop the second's node once the move has begun; hold it past the failover delay; let it go
/// on; wait for the move to settle. Expect the move moved or refused by kind ([`slates_faults::judge_move`]), every
/// acknowledged file served byte for byte, and the fleet re-formed.
#[test]
fn w7_a_move_whose_target_is_cut_off_midway_ends_moved_or_refused_by_kind() -> anyhow::Result<()> {
    if !enabled(Fault::Signal) {
        return Ok(());
    }
    let lab = Lab::start()?;
    let (agent, volume) = lab.start_agent(0)?;
    let acknowledged = write_acknowledged(&lab, 0, volume, "moved")?;
    let (from, to) = (lab.node(0)?.machine.clone(), lab.node(1)?.machine.clone());
    let held = lab.failover(2)?;
    let (code, body) = lab.fleet.request("POST", "/api/ops/agents.move", Some(&json!({"agent": agent, "to": to, "now": true})))?;
    lab.signal(1, "STOP")?;
    std::thread::sleep(held.saturating_add(held));
    lab.signal(1, "CONT")?;
    let refused = (!(200..300).contains(&code)).then(|| String::from_utf8_lossy(&body).into_owned());
    move_settles(&lab, &agent, &from, &to, refused.as_deref())?;
    no_acknowledged_change_lost(&lab, volume, &acknowledged, "a link lost mid-move")?;
    lab.until_reformed("after a link lost mid-move")
}

/// Waits until kiln's word on agent `agent`'s move `from -> to` settles on a verdict W7 allows
/// ([`slates_faults::judge_move`]), `refused` being kiln's answer to the ask where it refused it, and says which.
fn move_settles(lab: &Lab, agent: &str, from: &str, to: &str, refused: Option<&str>) -> anyhow::Result<MoveOutcome> {
    let began = Instant::now();
    let outcome = loop {
        lab.reroot();
        let runs_on = lab.runs_on(agent)?;
        // A move kiln took (`agents.move` answered `moving`) and its runner then refused says so in the agent's timeline
        // ("Couldn't move to …: … [kind: …] — it goes on here", `harness::session::move_failed`): kiln's word too.
        let in_timeline = if refused.is_none() { lab.move_refused_in_timeline(agent)? } else { None };
        let said = MoveSaid { from, to, runs_on: runs_on.as_deref(), refused: refused.or(in_timeline.as_deref()) };
        match slates_faults::judge_move(&said) {
            Ok(outcome) => break outcome,
            Err(why) => anyhow::ensure!(began.elapsed() < WITHIN, "the move settled on no verdict W7 allows: {why:?}\nkiln's word on the agent: {}\n{}", lab.agent(agent).map_or_else(|e| format!("{e:#}"), |v| v.to_string()), lab.slates_story()),
        }
        std::thread::sleep(LOOK);
    };
    match &outcome {
        MoveOutcome::Moved { to } => eprintln!("PROVEN: the move completed: the agent works on {to}"),
        MoveOutcome::Refused { kind } => eprintln!("PROVEN: the move refused by kind {kind}, the agent where it was"),
    }
    Ok(outcome)
}

/// W7, A-708: a slow, lossy link. Do: with the relay between the nodes, hold every datagram on every path 40 ms ± 10 ms
/// (so they reorder) and lose 1 %; write acknowledged files on the first runner over it; ask kiln to move the agent to
/// the second; let the move settle; heal the links. Expect the record links to have crossed the relay, the move moved or
/// refused by kind, every acknowledged file served byte for byte, and the fleet re-formed.
#[test]
fn w7_a708_a_slow_lossy_link_loses_no_acknowledged_change_and_a_move_over_it_ends_moved_or_refused_by_kind() -> anyhow::Result<()> {
    if !enabled(Fault::Link) {
        return Ok(());
    }
    let lab = Lab::start_relayed()?;
    let slow = relay::PathRule { loss_ppm: SLOW_LOSS_PPM, delay: SLOW_DELAY, jitter: SLOW_JITTER, ..relay::PathRule::default() };
    lab.relay()?.shape_all(slow);
    eprintln!("FAULT: every link held {SLOW_DELAY:?} ± {SLOW_JITTER:?}, {SLOW_LOSS_PPM} ppm lost");
    let (agent, volume) = lab.start_agent(0)?;
    let acknowledged = write_acknowledged(&lab, 0, volume, "slow")?;
    lab.crossed_the_relay()?;
    let (from, to) = (lab.node(0)?.machine.clone(), lab.node(1)?.machine.clone());
    let (code, body) = lab.fleet.request("POST", "/api/ops/agents.move", Some(&json!({"agent": agent, "to": to, "now": true})))?;
    eprintln!("MOVE: agents.move answered {code} {}", String::from_utf8_lossy(&body));
    let refused = (!(200..300).contains(&code)).then(|| String::from_utf8_lossy(&body).into_owned());
    move_settles(&lab, &agent, &from, &to, refused.as_deref())?;
    no_acknowledged_change_lost(&lab, volume, &acknowledged, "a slow, lossy link")?;
    lab.relay()?.heal();
    eprintln!("RELAY: {}", lab.relay()?.report().summary());
    no_acknowledged_change_lost(&lab, volume, &acknowledged, "a slow link healed")?;
    lab.until_reformed("after a slow, lossy link")
}

/// W7, A-708: a runner cut off while it runs, the partition SIGSTOP can't make (its processes run on, their leases
/// lapsing and their peers judged dead from their side). Do: with the relay between the nodes, write acknowledged files
/// on the first runner; cut every path into and out of its node past the fleet's failover delay; read the files through
/// the other two; heal. Expect the cut to have dropped datagrams, every acknowledged file served byte for byte through
/// the survivors while it's cut and through any node once healed, and the fleet re-formed.
#[test]
fn w7_a708_a_runner_cut_off_while_it_runs_loses_no_acknowledged_change_and_the_fleet_reforms() -> anyhow::Result<()> {
    if !enabled(Fault::Link) {
        return Ok(());
    }
    let lab = Lab::start_relayed()?;
    let (_agent, volume) = lab.start_agent(0)?;
    let acknowledged = write_acknowledged(&lab, 0, volume, "cut")?;
    // The control: the survivors serve the files before any fault, so a loss after the cut is the cut's.
    no_acknowledged_change_lost_through(&lab, &[1, 2], volume, &acknowledged, "read through the other runners before the cut")?;
    lab.crossed_the_relay()?;
    let held = lab.failover(1)?;
    lab.relay()?.isolate(0);
    eprintln!("FAULT: {} cut off from its peers, its processes running", lab.node(0)?.name);
    std::thread::sleep(held.saturating_add(held));
    no_acknowledged_change_lost_through(&lab, &[1, 2], volume, &acknowledged, "a runner cut off while it runs")?;
    let report = lab.relay()?.report();
    anyhow::ensure!(report.cut(0, 1).saturating_add(report.cut(1, 0)) > 0, "the cut dropped nothing: {}", report.summary());
    lab.relay()?.heal();
    eprintln!("RELAY: {}", lab.relay()?.report().summary());
    no_acknowledged_change_lost(&lab, volume, &acknowledged, "a cut-off runner back")?;
    lab.until_reformed("after a runner cut off")
}

/// The gate and the end's mount check, which run without the switch.
mod checks {
    use super::*;

    /// W7, A-708: the link faults run on any Unix and the signalling flows on Linux alone, each behind the switch; a skip
    /// says why.
    #[test]
    fn a708_the_link_faults_run_off_linux_and_the_signalled_ones_on_linux_alone() {
        assert_eq!(skip(Fault::Link, false, true), None, "a link fault runs on macOS");
        assert_eq!(skip(Fault::Link, true, true), None);
        assert_eq!(skip(Fault::Signal, true, true), None);
        assert!(skip(Fault::Signal, false, true).is_some_and(|w| w.contains("Linux alone")), "no daemon is signalled under a macOS NFS mount");
        assert_eq!(skip(Fault::Unmounted, false, true), None, "a daemon whose mounts have gone is signalled on macOS too");
        assert!(skip(Fault::Link, false, false).is_some_and(|w| w.contains(SWITCH)), "off without the switch");
        assert!(skip(Fault::Signal, true, false).is_some_and(|w| w.contains(SWITCH)));
    }

    /// W7, A-708: the lab's end finds the Slates mounts under its folder in either table, and no other.
    #[test]
    fn a708_the_labs_end_finds_its_slates_mounts_and_no_other() {
        let lab = Path::new("/private/var/folders/x/T/.tmpAb12");
        let bsd = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
                   slates:/kiln-01a1 on /private/var/folders/x/T/.tmpAb12/fleet-a/home/.kiln/workspaces/kiln 01a1 (nfs, nodev, nosuid, mounted by ada)\n\
                   slates:/persons-own on /Users/ada/mine (nfs, nodev, nosuid, mounted by ada)\n\
                   map auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)\n";
        assert_eq!(slates_mounts_under(bsd, false, lab), vec!["/private/var/folders/x/T/.tmpAb12/fleet-a/home/.kiln/workspaces/kiln 01a1".to_owned()]);
        let lab = Path::new("/tmp/.tmpAb12");
        let info = "36 35 98:0 / / rw,noatime master:1 - ext4 /dev/root rw\n\
                    90 36 0:51 / /tmp/.tmpAb12/fleet-b/home/.kiln/workspaces/kiln\\04001a1 rw,nosuid,nodev - fuse.slates slates:7 rw\n\
                    91 36 0:52 / /home/ada/mine rw - fuse.slates slates:8 rw\n";
        assert_eq!(slates_mounts_under(info, true, lab), vec!["/tmp/.tmpAb12/fleet-b/home/.kiln/workspaces/kiln 01a1".to_owned()]);
    }
}
