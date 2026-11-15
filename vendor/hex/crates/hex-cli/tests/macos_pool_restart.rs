//! AC-T7.4.1 on macOS: a Live managed pool across a killed node, through the real `hex node`. The node runs
//! with a Shards guest configured whose controller is a fixture script, never a VM, held by the persistent
//! managed helper (`hex native-managed-helper`, the same binary). Its guest is this test: a vsock front that
//! answers `CONNECT <port>` with `OK` and the relay's token, as Shards' muxer and `hex guest relay` do, then
//! joins the connection to a node served in this process over the simulator, whose capacity is the pool's
//! room. The pool is made Live through the node's socket (its offer granted), the node is SIGKILLed, and a
//! node started on the same state takes the pool back: with its controller still running, the same
//! process, re-attached and Live; with its controller ended while no node ran (its vsock gone with it)
//! and a descendant outliving it, Draining with its end recorded and the descendant ended. Either is then
//! released through the restarted node and nothing of it is left. Each test waits on the fact it needs,
//! within PATIENCE, and checks what is left by pid and birth.
#![cfg(target_os = "macos")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::io::{BufRead, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hex_api::sim::SimRuntime;
use hex_api::{
    BackingGrant, BackingKey, BackingOffer, BackingPhase, BoxId, BoxInfo, BoxSpec, Capacity,
    CapacityResources, Class, Proc, Program, Refusal, Runtime, RuntimeCost, Signal, Stdio, Usage,
};
use hex_node::serve::{Host, Server};
use sha2::{Digest, Sha256};

/// Longest a test waits for a fact it needs: a bound, so a node that never makes it true fails the test
/// instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(60);

/// The controller: argv as the SDK prepares it, plus the helper's `--host-lifeline FD`. It records itself
/// in the lab's `records` folder (`RECORDS`, written in when the lab writes the script), and its descendant
/// records itself there once it ignores SIGHUP, so it outlives the controller: a controller's end orphans
/// its group, which the kernel hangs up where a member is stopped (XNU's `orphanpg`), and one hung up
/// before its trap would end with the controller by chance. The descendant never reads the lifeline. The
/// controller then reads its lifeline until the helper's end closes it.
const CONTROLLER: &str = r#"#!/bin/sh
echo $$ > "RECORDS/controller.$$"
lifeline=""
while [ $# -gt 0 ]; do
  if [ "$1" = "--host-lifeline" ]; then lifeline="$2"; shift; fi
  shift
done
/bin/sh -c 'trap "" HUP; echo $$ > "$1"; exec /bin/sleep 100000' descendant "RECORDS/descendant.$$" &
eval "/bin/cat <&$lifeline" > /dev/null
"#;

/// The guest's node: the simulator's boxes, and the pool's room as its capacity, which the host checks a
/// booted pool against.
struct GuestNode {
    sim: SimRuntime,
    room: CapacityResources,
}

impl Runtime for GuestNode {
    fn classes(&self) -> Vec<Class> {
        self.sim.classes()
    }
    fn capacity(&self, class: Class) -> Result<Option<Capacity>, Refusal> {
        Ok((class == Class::Oci).then(|| Capacity {
            own: Default::default(),
            id: "guest".to_owned(),
            total: self.room,
            free: self.room,
            devices: Vec::new(),
            cost: None,
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            unavailable: Vec::new(),
            learned: None,
            node_vms: Vec::new(),
            unlearned: None,
        }))
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.sim.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.start(id)
    }
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.exec(id, program, stdio)
    }
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.sim.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.sim.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.sim.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.sim.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.delete(id)
    }
}

impl Host for GuestNode {
    fn abandoned(&self, _: &BoxId) -> Result<(), Refusal> {
        Ok(())
    }
    fn netns(&self) -> Result<hex_core::netns::Counts, Refusal> {
        Ok(hex_core::netns::Counts::default())
    }
}

/// The guest as the host reaches it: its node, served here, and the pool's vsock front once bound.
struct Guest {
    _server: Server,
    node: PathBuf,
    /// Control connections the front has joined to the guest's node.
    dials: Arc<AtomicU64>,
}

impl Guest {
    fn new(dir: &Path, room: CapacityResources) -> Self {
        let node = dir.join("guest.sock");
        let platform = hex_node::serve::macos::platform().unwrap();
        let server = Server::bind(
            &node,
            Arc::new(GuestNode {
                sim: SimRuntime::new(),
                room,
            }) as Arc<dyn Host>,
            hex_node::serve::limits(&platform),
            platform.threads,
        )
        .unwrap();
        Self {
            _server: server,
            node,
            dials: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Shards' muxer and `hex guest relay` at `vsock`: each connection's `CONNECT <port>` answered `OK`,
    /// a control port's then sent its token and joined to the guest's node; any other port closed.
    fn front(&self, vsock: &Path) {
        let listener = UnixListener::bind(vsock).unwrap();
        let (node, dials) = (self.node.clone(), Arc::clone(&self.dials));
        std::thread::spawn(move || {
            for (token, host) in (0u64..).zip(listener.incoming()) {
                let Ok(host) = host else { return };
                let (node, dials) = (node.clone(), Arc::clone(&dials));
                std::thread::spawn(move || relay(host, &node, token, &dials));
            }
        });
    }
}

fn relay(mut host: UnixStream, node: &Path, token: u64, dials: &AtomicU64) {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    // "CONNECT ", a u32's digits and its newline: the request the muxer reads, never more.
    loop {
        if host.read(&mut byte).unwrap_or(0) == 0 {
            return;
        }
        if byte[0] == b'\n' {
            break;
        }
        if line.len() == "CONNECT 4294967295".len() {
            return;
        }
        line.push(byte[0]);
    }
    let port = std::str::from_utf8(&line)
        .ok()
        .and_then(|line| line.strip_prefix("CONNECT "))
        .and_then(|port| port.parse::<u32>().ok());
    if port != Some(hex_core::stream::CONTROL_PORT) {
        return;
    }
    let answer = format!("OK {}\n", hex_core::stream::CONTROL_PORT);
    if host.write_all(answer.as_bytes()).is_err() || host.write_all(&token.to_le_bytes()).is_err() {
        return;
    }
    let Ok(guest) = UnixStream::connect(node) else {
        return;
    };
    dials.fetch_add(1, Ordering::SeqCst);
    let (to_guest, from_guest) = (host.try_clone().unwrap(), guest.try_clone().unwrap());
    let up = std::thread::spawn(move || copy(to_guest, guest));
    copy(from_guest, host);
    let _ = up.join();
}

/// Copies until `from` ends, then ends both: the relay's rule that either side's end is the other's.
fn copy(mut from: UnixStream, mut to: UnixStream) {
    let _ = std::io::copy(&mut from, &mut to);
    let _ = to.shutdown(std::net::Shutdown::Both);
    let _ = from.shutdown(std::net::Shutdown::Both);
}

/// One state folder, its configured guest, and the fixture controller. The folder is under
/// `~/.cache`, short enough for the pool's vsock path to fit a socket address.
struct Lab {
    dir: tempfile::TempDir,
    env: Vec<(&'static str, PathBuf)>,
    cost: RuntimeCost,
    guest: Guest,
}

impl Lab {
    fn new() -> Self {
        // A grant boots the pool's controller and dials its guest: a client wait past its W* tells its run-out
        // and follows its connection (decision 9), and each call carries this test's own bound
        // (`capacity_before`, [`within`]).
        let cache = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache");
        std::fs::create_dir_all(&cache).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("hx-")
            .tempdir_in(&cache)
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let records = dir.path().join("records");
        std::fs::create_dir(&records).unwrap();
        let controller = dir.path().join("controller");
        let script = CONTROLLER.replace("RECORDS", records.to_str().unwrap());
        std::fs::write(&controller, script).unwrap();
        std::fs::set_permissions(&controller, std::fs::Permissions::from_mode(0o700)).unwrap();
        // The cost evidence carries this host class's CPU-response row, as the node requires of a
        // managed VM: a fixture row, never a measured profile; no enforcement conclusion is drawn here.
        let response = Duration::from_micros(117);
        let row = hex_runtime::guest::CpuResponse {
            host: hex_runtime::macos::machine_model().unwrap(),
            samples: 1,
            p50: response,
            p90: response,
            p99: response,
            max: response,
            raw: [0; 32],
        };
        let mut evidence = hex_runtime::guest::cpu_response_section(&[row]).unwrap();
        evidence.extend_from_slice(b"fixture evidence\n");
        let memory_mib = shards_api::min_memory_mib(std::env::consts::ARCH);
        let fixed = CapacityResources {
            cpu_milli: 1000,
            mem_bytes: memory_mib << 20,
            disk_bytes: 0,
        };
        let cost = RuntimeCost {
            id: Sha256::digest(&evidence).into(),
            floor: fixed,
            ceiling: fixed,
            quantum: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            overhead: CapacityResources::default(),
            guest_overhead: CapacityResources::default(),
        };
        let cost_bytes = serde_json::to_vec(&cost).unwrap();
        std::fs::write(dir.path().join("evidence"), &evidence).unwrap();
        std::fs::write(dir.path().join("cost.json"), &cost_bytes).unwrap();
        let sha: String = Sha256::digest(&cost_bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let env = vec![
            (hex_runtime::guest::ENV_SHARDS_BIN, controller),
            (
                hex_runtime::guest::ENV_KERNEL,
                dir.path().join("kernel-not-opened"),
            ),
            (
                hex_runtime::guest::ENV_INITRD,
                dir.path().join("initrd-not-opened"),
            ),
            (hex_runtime::guest::ENV_CPUS, PathBuf::from("1")),
            (
                hex_runtime::guest::ENV_MEMORY_MIB,
                PathBuf::from(memory_mib.to_string()),
            ),
            (
                hex_runtime::guest::ENV_COST_FILE,
                dir.path().join("cost.json"),
            ),
            (hex_runtime::guest::ENV_COST_SHA256, PathBuf::from(sha)),
            (
                hex_runtime::guest::ENV_COST_EVIDENCE,
                dir.path().join("evidence"),
            ),
        ];
        let room = cost
            .allocate(CapacityResources::default())
            .unwrap()
            .guest()
            .unwrap();
        let guest = Guest::new(dir.path(), room);
        Self {
            dir,
            env,
            cost,
            guest,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// A `hex node` of this build on the lab's state, with its guest configured: ready, or the refusal it
    /// ended with.
    fn node(&self) -> Result<Node, String> {
        let socket = self.path("node.sock");
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_hex"));
        command
            .arg("node")
            .arg("--socket")
            .arg(&socket)
            .arg("--state")
            .arg(self.path("state"))
            .env_remove(hex_runtime::guest::ENV_GPU)
            .env_remove(hex_runtime::guest::ENV_GPU_OBSERVER_FILE)
            .env_remove(hex_runtime::guest::ENV_GPU_OBSERVER_SHA256)
            .env_remove(hex_runtime::guest::shares::ENV)
            .stdout(std::process::Stdio::piped())
            // A file, not a pipe: what the node said before it ended is all there once it has ended,
            // whoever else (its helper) holds the descriptor.
            .stderr(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.path("node.err"))
                    .unwrap(),
            );
        for (name, value) in &self.env {
            command.env(name, value);
        }
        let mut child = command.spawn().expect("start hex node");
        let stdout = child.stdout.take().unwrap();
        let (sender, ready) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = std::io::BufReader::new(stdout).read_line(&mut line);
            let _ = sender.send(line);
        });
        let line = ready.recv_timeout(PATIENCE).unwrap_or_default();
        if line.trim() == format!("ready {}", socket.display()) {
            return Ok(Node { child, socket });
        }
        let _ = child.kill();
        let status = child.wait().unwrap();
        Err(format!(
            "{status}: {}",
            std::fs::read_to_string(self.path("node.err")).unwrap()
        ))
    }

    /// The pool's vsock: beside the node's guest receipt, named for its scope (`hex-pool-<scope>.sock`).
    fn vsock(&self, key: BackingKey) -> PathBuf {
        let scope: String = key.generation.iter().map(|b| format!("{b:02x}")).collect();
        self.path(&format!("state/hex-pool-{scope}.sock"))
    }

    /// Every controller the helper ever started here, as each recorded itself.
    fn controllers(&self) -> Vec<i32> {
        let Ok(entries) = std::fs::read_dir(self.path("records")) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| {
                let name = entry.unwrap().file_name().into_string().unwrap();
                name.strip_prefix("controller.")
                    .map(|pid| pid.parse().unwrap())
            })
            .collect()
    }

    /// The controller's descendant, as it recorded itself once it ignores SIGHUP.
    fn descendant(&self, controller: i32) -> i32 {
        let path = self.path(&format!("records/descendant.{controller}"));
        wait_for("the descendant to record itself", || {
            std::fs::read_to_string(&path).is_ok_and(|text| text.ends_with('\n'))
        });
        std::fs::read_to_string(&path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// The helper's pid, from its private startup receipt: `HEXH`, version, then its pid.
    fn helper_pid(&self) -> i32 {
        let receipt = std::fs::read(self.path("state/managed-helper/helper.receipt")).unwrap();
        i32::try_from(u32::from_le_bytes(receipt[6..10].try_into().unwrap())).unwrap()
    }

    /// The node's shared pool, made Live through its socket: its Prepared offer granted, the guest's
    /// front bound at its vsock first. Returns the pool's key and its controller.
    fn live_pool(&self, node: &Node) -> (BackingKey, i32) {
        let client = node.client();
        let offer = shared_offer(&client);
        assert_eq!(offer.phase, BackingPhase::Prepared, "{offer:?}");
        assert_eq!(offer.cost, self.cost);
        self.guest.front(&self.vsock(offer.key));
        let grant = BackingGrant {
            key: offer.key,
            allocation: offer.cost.allocate(CapacityResources::default()).unwrap(),
            devices: Vec::new(),
        };
        let socket = node.socket.clone();
        let live = within("the grant", move || {
            hex_client::Client::connect(&socket)
                .unwrap()
                .initialize_backing(grant)
        })
        .expect("the shared pool initialized");
        assert_eq!(live.phase, BackingPhase::Live, "{live:?}");
        assert_eq!(live.key, offer.key);
        wait_for("the controller to record itself", || {
            !self.controllers().is_empty()
        });
        let controllers = self.controllers();
        assert_eq!(controllers.len(), 1, "{controllers:?}");
        (offer.key, controllers[0])
    }

    /// Ends what this lab started that is still running: its node first, then the helper (which ends
    /// its controllers' lifelines), then any descendant left. Each by the pid and birth seen.
    fn end(&self, node: Option<Node>, descendants: &[(i32, u64)]) {
        drop(node);
        if self.path("state/managed-helper/helper.receipt").exists() {
            let helper = self.helper_pid();
            if alive(helper) {
                let _ = rustix::process::kill_process(
                    rustix::process::Pid::from_raw(helper).unwrap(),
                    rustix::process::Signal::KILL,
                );
            }
        }
        for &(pid, started) in descendants {
            if birth(pid) == Some(started) && alive(pid) {
                let _ = rustix::process::kill_process(
                    rustix::process::Pid::from_raw(pid).unwrap(),
                    rustix::process::Signal::KILL,
                );
            }
        }
    }
}

/// A `hex node` of this build, ended by SIGKILL when dropped (a crash, as the test means one).
struct Node {
    child: std::process::Child,
    socket: PathBuf,
}

impl Node {
    fn client(&self) -> hex_client::Client {
        hex_client::Client::connect(&self.socket).expect("connect to hex node")
    }

    /// SIGKILL, and reaped.
    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The node's one shared CPU pool, as its `oci` capacity offers it.
fn shared_offer(client: &hex_client::Client) -> BackingOffer {
    let began = Instant::now();
    let capacity = client
        .capacity_before(Class::Oci, began + PATIENCE)
        .unwrap()
        .expect("the node's oci capacity");
    eprintln!("the node's oci capacity answered in {:?}", began.elapsed());
    let mut backings = capacity.backings;
    assert_eq!(backings.len(), 1, "{backings:?}");
    backings.remove(0)
}

/// The node's release of pool `key`, within PATIENCE.
fn release(node: &Node, key: BackingKey) -> Result<BackingOffer, Refusal> {
    let socket = node.socket.clone();
    within("the release", move || {
        hex_client::Client::connect(&socket)
            .unwrap()
            .release_backing(key)
    })
}

/// Pool `key` released through a node restarted on the helper's state. That node isn't the helper's
/// parent (launchd is, since the first node ended): the helper's exit after its last scope's finalize is
/// seen, its reap isn't, and its absence is what finalize proves (remote's `finish_helper`), so the
/// release may be refused by type for the Index's next try. This is that next try, once the helper is
/// gone.
fn released_by(node: &Node, key: BackingKey, helper: i32) -> BackingOffer {
    match release(node, key) {
        Err(Refusal::Invalid { what, why })
            if what == "managed native configuration"
                && why == "helper exit observed but native absence/reap is not yet proven" =>
        {
            wait_for("the helper to be reaped", || absent(helper));
            release(node, key)
        }
        other => other,
    }
    .expect("the pool released")
}

/// One call to the node on a thread of its own, answered within PATIENCE: a node that never answers
/// fails the test instead of hanging it (the client's own waits measure here, [`Lab::new`]).
fn within<T: Send + 'static>(what: &str, call: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, answer) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(call());
    });
    answer
        .recv_timeout(PATIENCE)
        .unwrap_or_else(|_| panic!("{what}: no answer within {PATIENCE:?}"))
}

fn birth(pid: i32) -> Option<u64> {
    hex_runtime::macos::sys::rusage(pid)
        .unwrap()
        .map(|r| r.started)
}

fn alive(pid: i32) -> bool {
    hex_runtime::macos::sys::rusage(pid)
        .unwrap()
        .is_some_and(|r| !r.ended)
}

/// Gone: no process with this pid at all, not even a zombie.
fn absent(pid: i32) -> bool {
    hex_runtime::macos::sys::rusage(pid).unwrap().is_none()
}

fn wait_for(what: &str, mut fact: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !fact() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::park_timeout(Duration::from_millis(20));
    }
}

/// AC-T7.4.1 (restart, controller alive): make the node's shared pool Live, SIGKILL the node, and start
/// another on the same state: expect it ready, the pool Live under the same key, its controller the
/// same process (pid and birth) and never a second one, and the guest dialed again. Then release the
/// pool through the restarted node: expect it Released, its controller and descendant gone, and the
/// helper ended with its last scope.
#[test]
fn a_restarted_node_takes_back_a_live_pool_whose_controller_runs() {
    let lab = Lab::new();
    let node = lab.node().expect("the first node ready");
    let (key, controller) = lab.live_pool(&node);
    let started = birth(controller).unwrap();
    let descendant = lab.descendant(controller);
    let descendants = [(descendant, birth(descendant).unwrap_or_default())];
    let helper = lab.helper_pid();
    node.kill();
    assert!(alive(controller), "the controller ended with its node");
    assert!(alive(helper), "the helper ended with its node");
    let dials = lab.guest.dials.load(Ordering::SeqCst);

    let node = match lab.node() {
        Ok(node) => node,
        Err(why) => {
            lab.end(None, &descendants);
            panic!("the restarted node refused its Live pool: {why}");
        }
    };
    let client = node.client();
    let offer = shared_offer(&client);
    assert_eq!((offer.key, offer.phase), (key, BackingPhase::Live));
    assert_eq!(
        birth(controller),
        Some(started),
        "the controller isn't the same process"
    );
    assert!(alive(controller), "the controller ended across the restart");
    assert_eq!(
        lab.controllers(),
        vec![controller],
        "a second controller ran"
    );
    assert_eq!(lab.helper_pid(), helper, "a second helper ran");
    assert!(
        lab.guest.dials.load(Ordering::SeqCst) > dials,
        "the restarted node never dialed its guest"
    );

    let released = released_by(&node, key, helper);
    assert_eq!(released.phase, BackingPhase::Released, "{released:?}");
    wait_for("the controller to be gone", || absent(controller));
    wait_for("the descendant to be gone", || !alive(descendant));
    wait_for("the helper to end with its last scope", || absent(helper));
    lab.end(Some(node), &descendants);
}

/// AC-T7.4.1 (restart, controller gone): make the node's shared pool Live, SIGKILL the node, then its
/// controller while no node runs, its descendant outliving it; start another node on the same state:
/// expect it ready, never dialing the gone VM's guest, with the pool Draining, its end recorded, the
/// descendant ended by its recovery, never Live and never a second controller; then release it: expect it
/// Released, nothing of its scope left, and the helper ended with it.
#[test]
fn a_restarted_node_releases_a_live_pool_whose_controller_ended() {
    let lab = Lab::new();
    let node = lab.node().expect("the first node ready");
    let (key, controller) = lab.live_pool(&node);
    let started = birth(controller).unwrap();
    let descendant = lab.descendant(controller);
    let descendants = [(descendant, birth(descendant).unwrap_or_default())];
    let helper = lab.helper_pid();
    node.kill();
    assert_eq!(birth(controller), Some(started));
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(controller).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    // Its vsock goes with it, as a VMM's does.
    std::fs::remove_file(lab.vsock(key)).unwrap();
    // The helper, its parent, keeps it unreaped until a node finalizes its scope.
    wait_for("the controller to end", || !alive(controller));

    let node = match lab.node() {
        Ok(node) => node,
        Err(why) => {
            lab.end(None, &descendants);
            panic!("the restarted node refused a pool whose controller ended: {why}");
        }
    };
    let client = node.client();
    let offer = shared_offer(&client);
    assert_eq!((offer.key, offer.phase), (key, BackingPhase::Draining));
    assert!(
        !alive(descendant),
        "the restarted node left the ended pool's descendant running"
    );
    assert_eq!(
        lab.controllers(),
        vec![controller],
        "a second controller ran"
    );
    assert_eq!(lab.helper_pid(), helper, "a second helper ran");
    let released = released_by(&node, key, helper);
    assert_eq!(released.phase, BackingPhase::Released, "{released:?}");
    wait_for("the controller to be reaped", || absent(controller));
    wait_for("the descendant to be gone", || !alive(descendant));
    wait_for("the helper to end with its last scope", || absent(helper));
    lab.end(Some(node), &descendants);
}
