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
//! It judges what holds, not how fast: no latency is judged. Each flow waits on the fleet's own failover delay as its
//! nodes report it (`daemon_status`), never a number of its own. Gated by `SLATES_FLEET_FAULTS=1`, since each flow starts
//! three Slates instances and their anchors; without it each skips, saying so. Linux alone, where a workspace is a FUSE
//! mount: on macOS it's a kernel NFS mount over loopback, and a daemon killed or stopped under a live NFS mount has
//! panicked the Mac (Slates' record of 2026-10-06), so the macOS flows wait until a runner lets its workspace mounts go
//! before a fault.
//!
//!   SLATES_FLEET_FAULTS=1 cargo test -p kiln --test slates_fleet_faults -- --nocapture --test-threads 1

#![cfg(target_os = "linux")]

mod common;
mod vm_e2e;

use std::collections::BTreeMap;
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

/// Whether the flows run here; a skip says why.
fn enabled() -> bool {
    if std::env::var_os(SWITCH).is_some_and(|v| v == "1") {
        return true;
    }
    eprintln!("skipped: {SWITCH}=1 runs kiln's Slates fleet under faults (three Slates instances and their anchors)");
    false
}

/// One runner of the lab: the name it was started as, its machine key (what `[slates_fleet.nodes]` lists), its Slates
/// instance and where its node is reached.
struct Node {
    name: &'static str,
    machine: String,
    instance: String,
    address: String,
}

/// The lab: a server of the test's own and three runners forming kiln's Slates fleet.
struct Lab {
    fleet: common::Fleet,
    nodes: Vec<Node>,
    project: PathBuf,
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
            nodes.push(Node { name, machine: common::machine(name), instance: format!("kilnflt{run}{name}").replace('-', ""), address: format!("127.0.0.1:{port}") });
        }
        let listed: String = nodes.iter().map(|n| format!("\"{}\" = \"{}\"\n", n.machine, n.address)).collect();
        let extra = format!("[slates_fleet]\nf = 1\n[slates_fleet.nodes]\n{listed}");
        let mut fleet = common::Fleet::with(move |_| extra.clone())?;
        fleet.start_server()?;
        let project = fleet.root.path().join("project");
        make_project(&project)?;
        let mut lab = Lab { fleet, nodes, project };
        for i in 0..lab.nodes.len() {
            lab.start_runner(i)?;
        }
        lab.until_reformed("the fleet's first forming")?;
        Ok(lab)
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
        env.extend([("KILN_SLATES_INSTANCE", instance), ("KILN_SLATES_DIRECT", "1".to_owned()), ("KILN_SLATES", String::new())]);
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.fleet.start_runner_with(name, &env)
    }

    /// Waits until every listed runner's node serves as a member ([`slates_faults::not_reformed`]), saying `what`.
    fn until_reformed(&self, what: &str) -> anyhow::Result<()> {
        let listed: Vec<&str> = self.nodes.iter().map(|n| n.machine.as_str()).collect();
        let began = Instant::now();
        loop {
            let members = self.members();
            let missing = slates_faults::not_reformed(&listed, &members);
            if missing.is_empty() {
                eprintln!("PROVEN: {what}: every listed runner's node serves as a member ({:?})", began.elapsed());
                return Ok(());
            }
            anyhow::ensure!(began.elapsed() < WITHIN, "{what}: the fleet didn't re-form within {WITHIN:?}; not serving: {missing:?}\n{}", self.fleet.logs());
            std::thread::sleep(LOOK);
        }
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
        let spec = json!({"kind": "claude", "on": on, "cwd": self.project, "prompt": "wait", "workspace": {"kind": "volume"}});
        let started = self.fleet.call("POST", "/api/ops/agents.start", Some(&spec))?;
        let id = ["/id", "/session/id", "/agent/id", "/agent"]
            .iter()
            .find_map(|p| started.pointer(p).and_then(Value::as_str))
            .ok_or_else(|| anyhow::anyhow!("kiln named no session for the agent it started: {started}"))?
            .to_owned();
        let began = Instant::now();
        loop {
            let info = self.agent(&id)?;
            if let Some(volume) = info.pointer("/workspace/made/volume").and_then(Value::as_str).filter(|v| !v.trim().is_empty()) {
                return Ok((id, volume_id(volume)?));
            }
            anyhow::ensure!(began.elapsed() < WITHIN, "agent {id}'s workspace was never made: {info}");
            std::thread::sleep(LOOK);
        }
    }

    /// kiln's word on agent `id` (`agents.get`).
    fn agent(&self, id: &str) -> anyhow::Result<Value> {
        self.fleet.call("POST", "/api/ops/agents.get", Some(&json!({"agent": id})))
    }

    /// Where agent `id` works now, by kiln's word: its runner's machine key.
    fn runs_on(&self, id: &str) -> anyhow::Result<Option<String>> {
        let info = self.agent(id)?;
        Ok(["/on", "/runner", "/machine", "/session/on"].iter().find_map(|p| info.pointer(p).and_then(Value::as_str)).map(str::to_owned))
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
        // A stopped instance is let go on first, so its end is heard; then every instance the lab started is ended.
        for i in 0..self.nodes.len() {
            let _ = self.signal(i, "CONT");
            let _ = self.signal(i, "TERM");
        }
    }
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
    for k in 0..FILES {
        let path = format!("/faults/{round}-{k}.txt");
        let bytes = format!("{round} {k}: acknowledged before the fault\n").into_bytes();
        if instance.client().fs_write((volume, attachment), &path, &bytes, 0o644).is_ok() {
            acknowledged.insert(path, bytes);
        }
    }
    let _ = instance.detach(attachment);
    anyhow::ensure!(acknowledged.len() == FILES, "only {} of {FILES} writes were acknowledged before the fault", acknowledged.len());
    Ok(acknowledged)
}

/// Every acknowledged file read back through each node in turn, until one serves the volume: what is served now.
fn served(lab: &Lab, volume: slates_api::VolumeId, paths: &BTreeMap<String, Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    let mut found = BTreeMap::new();
    for path in paths.keys() {
        for n in &lab.nodes {
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
    let began = Instant::now();
    loop {
        let lost = slates_faults::lost(acknowledged, &served(lab, volume, acknowledged));
        if lost.is_empty() {
            eprintln!("PROVEN: {what}: every one of {} acknowledged files served byte for byte ({:?})", acknowledged.len(), began.elapsed());
            return Ok(());
        }
        anyhow::ensure!(began.elapsed() < WITHIN, "{what}: acknowledged changes lost: {lost:?}\n{}", lab.fleet.logs());
        std::thread::sleep(LOOK);
    }
}

/// W7, A-580: a runner killed. Do: write acknowledged files into the agent's volume on the first runner; kill its
/// instance's anchor and daemon at once (the machine's loss); read them back through the others; start the runner again.
/// Expect every acknowledged file served byte for byte after the fault, and the fleet re-formed once it's back.
#[test]
fn w7_a_killed_runner_loses_no_acknowledged_change_and_the_fleet_reforms() -> anyhow::Result<()> {
    if !enabled() {
        return Ok(());
    }
    let mut lab = Lab::start()?;
    let (_agent, volume) = lab.start_agent(0)?;
    let acknowledged = write_acknowledged(&lab, 0, volume, "killed")?;
    lab.signal(0, "KILL")?;
    no_acknowledged_change_lost(&lab, volume, &acknowledged, "a runner killed")?;
    let name = lab.node(0)?.name;
    lab.fleet.stop_runner(name);
    lab.start_runner(0)?;
    lab.until_reformed("after a runner killed")
}

/// W7, A-580: a runner partitioned. Do: write acknowledged files on the first runner; stop its instance's anchor and
/// daemon past the fleet's failover delay as its nodes report it; let them go on. Expect every acknowledged file served
/// byte for byte throughout what follows, and the fleet re-formed.
#[test]
fn w7_a_partitioned_runner_loses_no_acknowledged_change_and_the_fleet_reforms() -> anyhow::Result<()> {
    if !enabled() {
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
    if !enabled() {
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
    if !enabled() {
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
    let began = Instant::now();
    let outcome = loop {
        let runs_on = lab.runs_on(&agent)?;
        let said = MoveSaid { from: &from, to: &to, runs_on: runs_on.as_deref(), refused: refused.as_deref() };
        match slates_faults::judge_move(&said) {
            Ok(outcome) => break outcome,
            Err(why) => anyhow::ensure!(began.elapsed() < WITHIN, "the move settled on no verdict W7 allows: {why:?}\n{}", lab.fleet.logs()),
        }
        std::thread::sleep(LOOK);
    };
    match &outcome {
        MoveOutcome::Moved { to } => eprintln!("PROVEN: the move completed: the agent works on {to}"),
        MoveOutcome::Refused { kind } => eprintln!("PROVEN: the move refused by kind {kind}, the agent where it was"),
    }
    no_acknowledged_change_lost(&lab, volume, &acknowledged, "a link lost mid-move")?;
    lab.until_reformed("after a link lost mid-move")
}
