//! AC-T7.4.1: a CPU-only Firecracker microVM through the real `hex node` and its `firecracker`
//! class: the native lane's smoke boot before any GPU is bound (no device, no network); and, with
//! a VFIO-bound GPU (`gpu` inputs), the GPU run: a memlock below the VM's RAM refused with its DMA
//! error, then a GPU VM that computes, survives its node's restart and returns its GPU.
//! Explicit inputs only (`HEX_NATIVE_FC_SMOKE`, a JSON file); run alone, `--ignored`, inside a
//! cgroup delegated to this binary (`HEX_DELEGATED`) whose outer scope bounds the whole process.
//! Missing hardware or configuration fails by name; nothing here enables hardware.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio as HostStdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hex_api::{
    Bounds, BoxId, BoxSpec, BoxState, CapacityResources, Class, ClassSpec, DeviceId, DeviceKind,
    Exit, Network, OciSpec, Program, Refusal, Rootfs, Runtime, Stdio,
};
use hex_client::Client;
use serde_json::Value;

struct Inputs {
    value: Value,
    wait: Duration,
}

impl Inputs {
    fn load() -> Self {
        assert!(
            std::env::var_os("HEX_DELEGATED").is_some(),
            "run inside a cgroup delegated to this binary (HEX_DELEGATED)"
        );
        let path = std::env::var_os("HEX_NATIVE_FC_SMOKE").expect("HEX_NATIVE_FC_SMOKE inputs");
        let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let wait = Duration::from_millis(value["wait_ms"].as_u64().unwrap());
        assert!(!wait.is_zero());
        // A VM's create boots it: the client's learned create bound (twice the recorded slowest,
        // an OCI box's) would refuse it first, so every wait here measures and each call that
        // makes a VM carries this test's explicit deadline instead. Only until L4 gives each
        // class its own answer patience; then this goes, and create_before alone remains.
        hex_core::patience::measure_all();
        Self { value, wait }
    }

    fn path(&self, field: &str) -> PathBuf {
        PathBuf::from(
            self.value[field]
                .as_str()
                .unwrap_or_else(|| panic!("input {field}")),
        )
    }
}

/// This binary's own cgroup, made a parent: it moves into a leaf, as node_socket does (once).
fn own_cgroup() -> PathBuf {
    let line = fs::read_to_string("/proc/self/cgroup").unwrap();
    let own = Path::new("/sys/fs/cgroup").join(line.trim().strip_prefix("0::/").unwrap());
    if own.file_name().is_some_and(|name| name == "hex-fc-smoke") {
        return own.parent().unwrap().to_path_buf();
    }
    let leaf = own.join("hex-fc-smoke");
    fs::create_dir_all(&leaf).unwrap();
    fs::write(leaf.join("cgroup.procs"), std::process::id().to_string()).unwrap();
    own
}

/// A `hex node` with the firecracker class, ended by its own PID when dropped.
struct Node {
    child: Child,
    socket: PathBuf,
    state: PathBuf,
}

impl Node {
    fn start(inputs: &Inputs, own: &Path, name: &str, config: &Path) -> Self {
        Self::start_with(inputs, own, name, config, None)
    }

    /// A node whose device catalog (its state's `devices` file) is `catalog`, where given.
    fn start_with(
        inputs: &Inputs,
        own: &Path,
        name: &str,
        config: &Path,
        catalog: Option<&str>,
    ) -> Self {
        let dir = inputs.path("work").join(name);
        assert!(!dir.exists(), "exclusive node directory {dir:?}");
        fs::create_dir_all(dir.join("state")).unwrap();
        if let Some(catalog) = catalog {
            fs::write(dir.join("state").join("devices"), catalog).unwrap();
        }
        Self::run(inputs, own, name, config, "node.stderr")
    }

    /// Ends this node by its own PID (SIGKILL, as a crash ends one), leaving its boxes' VMs running, and starts
    /// another on the same socket, state and box root: the restart its recovery answers.
    fn restart(mut self, inputs: &Inputs, own: &Path, name: &str, config: &Path) -> Self {
        self.child.kill().unwrap();
        drop(self);
        Self::run(inputs, own, name, config, "node.stderr.restarted")
    }

    fn run(inputs: &Inputs, own: &Path, name: &str, config: &Path, stderr: &str) -> Self {
        let dir = inputs.path("work").join(name);
        let socket = dir.join("node.sock");
        let state = dir.join("state");
        let begun = Instant::now();
        let mut child = Command::new(inputs.path("hex"))
            .arg("node")
            .arg("--socket")
            .arg(&socket)
            .arg("--state")
            .arg(&state)
            .arg("--box-root")
            .arg(own.join(format!("hex-fc-{name}")))
            .env("HEX_FIRECRACKER_CONFIG", config)
            .stdin(HostStdio::null())
            .stdout(HostStdio::piped())
            .stderr(fs::File::create(dir.join(stderr)).unwrap())
            .spawn()
            .unwrap();
        // The node's own "ready <socket>" line is the fact waited on; the deadline only guards it.
        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let want = format!("ready {}", socket.display());
        let until = begun + inputs.wait;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) if line == want => break,
                Ok(line) => eprintln!("node {name}: {line}"),
                Err(error) => panic!(
                    "node {name} not ready: {error}; stderr: {}",
                    fs::read_to_string(dir.join(stderr)).unwrap_or_default()
                ),
            }
        }
        eprintln!(
            "SMOKE node {name} ready after {:?} (pid {})",
            begun.elapsed(),
            child.id()
        );
        Self {
            child,
            socket,
            state,
        }
    }

    fn client(&self) -> Client {
        Client::connect(&self.socket).unwrap()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let pid = self.child.id();
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        let status = self.child.wait().unwrap();
        eprintln!("SMOKE node pid {pid} ended: {status}");
    }
}

/// The box's spec: the class's payload, with its admitted charge from the backend's own cost.
fn spec(inputs: &Inputs, config: &Path, id: &BoxId, init: &[&str]) -> BoxSpec {
    let config = hex_runtime::firecracker::Config::from_file(config).unwrap();
    let cpu_milli = inputs.value["cpu_milli"].as_u64().unwrap();
    let mem_bytes = inputs.value["memory_bytes"].as_u64().unwrap();
    let allocation = config
        .cost
        .allocate(CapacityResources {
            cpu_milli,
            mem_bytes,
            disk_bytes: 0,
        })
        .unwrap();
    BoxSpec {
        id: id.clone(),
        bounds: Bounds {
            cpu_max_milli: Some(u32::try_from(cpu_milli).unwrap()),
            memory_max: Some(mem_bytes),
            ..Bounds::default()
        },
        class: ClassSpec::Firecracker(Box::new(OciSpec {
            rootfs: Rootfs::Dir(inputs.path("rootfs")),
            init: Program::of(init),
            mounts: Vec::new(),
            hostname: None,
            network: Network::Private,
            hosts: Vec::new(),
        })),
        owner: None,
        devices: Vec::new(),
        allocation: Some(allocation),
        route: None,
    }
}

/// The live VMM of the VM whose state directory is `dir`: its PID and start time, read from /proc
/// (observation only; the node alone stops it).
fn vmm(binary: &Path, dir: &Path) -> Vec<(u32, u64)> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap() {
        let entry = entry.unwrap();
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let words: Vec<_> = cmdline.split(|b| *b == 0).collect();
        let exe = words
            .first()
            .map(|w| Path::new(std::str::from_utf8(w).unwrap_or("")));
        let names_dir = words
            .iter()
            .any(|w| std::str::from_utf8(w).is_ok_and(|w| w.starts_with(&*dir.to_string_lossy())));
        if exe == Some(binary) && names_dir {
            let stat = fs::read_to_string(entry.path().join("stat")).unwrap();
            let start = stat
                .rsplit(") ")
                .next()
                .unwrap()
                .split(' ')
                .nth(19)
                .unwrap();
            found.push((pid, start.parse().unwrap()));
        }
    }
    found
}

fn alive(pid: u32, start: u64) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit(") ")
                .next()
                .and_then(|rest| rest.split(' ').nth(19))
                .map(|s| s.parse::<u64>().ok() == Some(start))
        })
        .unwrap_or(false)
}

/// Whether `pid`, started at `start`, still runs: there and not a zombie (`Z`), which holds nothing.
fn running(pid: u32, start: u64) -> bool {
    alive(pid, start)
        && fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit(") ").next().map(|rest| !rest.starts_with('Z')))
            .unwrap_or(false)
}

/// The VMMs of `binary` naming `dir` (the backend's state), ended by their own PID and start time when this is
/// dropped, however the test ends: a VMM outliving its killed node is the adoption path's to take back, never a
/// failed test's to leave holding its devices. A VMM that ended already is left alone.
struct Reaper {
    binary: PathBuf,
    dir: PathBuf,
    wait: Duration,
}

impl Drop for Reaper {
    fn drop(&mut self) {
        for (pid, start) in vmm(&self.binary, &self.dir) {
            if !running(pid, start) {
                continue;
            }
            // Its VM's directory in the backend's state, read from its command before it ends.
            let vm = vm_of(pid, &self.dir);
            let Some(target) = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()) else {
                continue;
            };
            let _ = rustix::process::kill_process(target, rustix::process::Signal::KILL);
            // Read again as long as the last read took, until it is gone or the run's wait has passed.
            let until = Instant::now() + self.wait;
            while running(pid, start) && Instant::now() < until {
                let read = Instant::now();
                let _ = running(pid, start);
                std::thread::park_timeout(read.elapsed());
            }
            let ended = !running(pid, start);
            // A VMM that ended leaves its VM's directory to whoever removes the VM: here, no node will.
            if let Some(vm) = vm.filter(|_| ended) {
                let _ = fs::remove_dir_all(&vm);
                eprintln!(
                    "SMOKE reaper: VM directory {} removed: {}",
                    vm.display(),
                    !vm.exists()
                );
            }
            eprintln!(
                "SMOKE reaper: VMM {pid} (start {start}) ended by the test's cleanup; running: {}",
                !ended
            );
        }
    }
}

/// The VM directory in `state` that VMM `pid`'s command names: its `--config-file`'s directory, where that lies
/// directly in `state`.
fn vm_of(pid: u32, state: &Path) -> Option<PathBuf> {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let mut words = cmdline
        .split(|b| *b == 0)
        .map(|word| std::str::from_utf8(word).unwrap_or(""));
    words.by_ref().find(|word| *word == "--config-file")?;
    let vm = Path::new(words.next()?).parent()?;
    (vm.parent() == Some(state)).then(|| vm.to_path_buf())
}

impl Reaper {
    fn of(config: &Path, wait: Duration) -> Self {
        let config = hex_runtime::firecracker::Config::from_file(config).unwrap();
        Self {
            binary: config.binary,
            dir: config.state_dir,
            wait,
        }
    }
}

/// The host's TUN/TAP interfaces (those with `tun_flags`): what a Firecracker network would add.
/// Other workloads' interfaces (a container's veth) come and go on a shared host.
fn taps() -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir("/sys/class/net")
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|path| path.join("tun_flags").exists())
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every file named `<id>.json` under the node's state: its records of the box (store.rs).
fn records(state: &Path, id: &BoxId) -> Vec<PathBuf> {
    let name = format!("{id}.json");
    let mut found = Vec::new();
    let mut pending = vec![state.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                pending.push(path);
            } else if entry.file_name().to_string_lossy() == name {
                found.push(path);
            }
        }
    }
    found
}

/// The one VM directory a single box leaves in the backend's state while it runs.
fn vm_dir(config: &Path) -> PathBuf {
    let config = hex_runtime::firecracker::Config::from_file(config).unwrap();
    let mut dirs: Vec<_> = fs::read_dir(&config.state_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(dirs.len(), 1, "one VM directory: {dirs:?}");
    dirs.pop().unwrap()
}

fn state_dir_entries(config: &Path) -> Vec<PathBuf> {
    let config = hex_runtime::firecracker::Config::from_file(config).unwrap();
    fs::read_dir(&config.state_dir)
        .map(|entries| entries.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default()
}

/// AC-T7.4.1: do boot one CPU-only Firecracker VM through the real node and adapter, run a
/// program in its box and delete it; expect readiness, the program's exit and output, a guest
/// that lists no PCI GPU function and was given no GPU words, and then no VMM process, no VM
/// state (its vsock and API sockets), no new TUN/TAP interface and no record of the box left.
#[test]
#[ignore = "native: KVM, a delegated cgroup and explicit pinned inputs (HEX_NATIVE_FC_SMOKE)"]
fn native_firecracker_cpu_box_runs_and_leaves_nothing() {
    let inputs = Inputs::load();
    let own = own_cgroup();
    let config = inputs.path("config");
    let binary = hex_runtime::firecracker::Config::from_file(&config)
        .unwrap()
        .binary;
    let before = taps();
    let _reaper = Reaper::of(&config, inputs.wait);
    let node = Node::start(&inputs, &own, "a", &config);
    let client = node.client();
    assert!(
        client.classes().contains(&Class::Firecracker),
        "{:?}",
        client.classes()
    );
    let id = BoxId::new("fc-smoke").unwrap();
    let begun = Instant::now();
    let info = client
        .create_before(
            spec(&inputs, &config, &id, &["/probe", "--hold"]),
            Instant::now() + inputs.wait,
        )
        .unwrap();
    eprintln!(
        "SMOKE create (boot to ready, guest box made) {:?}",
        begun.elapsed()
    );
    assert_eq!(info.class, Class::Firecracker);
    if client.inspect(&id).unwrap().state == BoxState::Created {
        client.start(&id).unwrap();
    }
    let dir = vm_dir(&config);
    let running = vmm(&binary, &dir);
    assert_eq!(running.len(), 1, "one live VMM for the box: {running:?}");
    let boot = fs::read_to_string(dir.join("config.json")).unwrap();
    let boot: Value = serde_json::from_str(&boot).unwrap();
    let boot = boot["boot-source"]["boot_args"]
        .as_str()
        .unwrap()
        .to_owned();
    eprintln!(
        "SMOKE VMM pid {} start {}; boot_args {boot:?}",
        running[0].0, running[0].1
    );
    assert!(
        !boot.contains("hex.gpu"),
        "a VM given no GPU carries no GPU words"
    );

    // GPU run r1119 (AC-T7.4.1; original): a program that runs until the test ends it. Its end is the guest's message:
    // the node never takes the guest's pid for a process of this host's, which named none there, so the node waited for
    // it as for a gone one, for its own wait for a box to empty, and told its end `Unknown` while it still ran. No end
    // comes within twice that wait's record (the suites' 904.7 ms), and the one its kill makes comes as the guest's.
    let mut held = client
        .exec(&id, Program::of(&["/probe", "--hold"]), Stdio::null())
        .unwrap();
    let early = held
        .wait_for(hex_testkit::waits::EMPTYING.saturating_mul(2))
        .unwrap();
    eprintln!("SMOKE held program before its kill: {early:?}");
    assert_eq!(early, None, "an end told while the guest's program runs");
    held.kill().unwrap();
    let killed = held.wait_for(inputs.wait).unwrap();
    eprintln!("SMOKE held program after its kill: {killed:?}");
    assert_eq!(killed, Some(Exit::Signal(9)));

    let challenge = inputs.value["challenge"].as_str().unwrap();
    let begun = Instant::now();
    let (mut out, writer) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &id,
            Program::of(&["/probe", challenge]),
            Stdio {
                stdout: Some(writer),
                ..Stdio::null()
            },
        )
        .unwrap();
    // The program's output to its end, which comes when it ends; the outer scope bounds a hang.
    let mut said = String::new();
    out.read_to_string(&mut said).unwrap();
    let exit = proc.wait_for(inputs.wait).unwrap();
    eprintln!("SMOKE exec {:?}: {exit:?} {said:?}", begun.elapsed());
    assert_eq!(exit, Some(Exit::Code(0)));
    let mut lines = said.lines();
    assert_eq!(lines.next(), Some(challenge));
    let pci = lines.next().unwrap();
    assert!(
        pci.starts_with("pci-functions ") && pci.ends_with(" pci-gpu-functions 0"),
        "{pci}"
    );
    drop(proc);

    let begun = Instant::now();
    client.delete(&id).unwrap();
    eprintln!("SMOKE delete {:?}", begun.elapsed());
    assert!(client.list().unwrap().is_empty(), "the node lists no box");
    let (pid, start) = running[0];
    assert!(!alive(pid, start), "the VMM {pid} is gone");
    assert!(
        vmm(&binary, &dir).is_empty(),
        "no VMM names the VM's directory"
    );
    assert!(
        !dir.exists(),
        "the VM's state (vsock, API and readiness sockets) is gone"
    );
    assert!(
        state_dir_entries(&config).is_empty(),
        "{:?}",
        state_dir_entries(&config)
    );
    assert_eq!(taps(), before, "no TUN/TAP interface was left");
    drop(client);
    let state = node.state.clone();
    drop(node);
    let left = records(&state, &id);
    assert!(
        left.is_empty(),
        "the node's records of the box are closed: {left:?}"
    );
    eprintln!(
        "SMOKE cleanup: VMM {pid} absent, {dir:?} absent, TUN/TAP unchanged ({} before), no {id}.json under {state:?}",
        before.len()
    );
}

/// AC-T7.4.1: do create a box on a node whose configured kernel is not a kernel, so its VMM ends
/// before the guest is ready; expect the create refused Broken with the end of what the VMM wrote,
/// and nothing of the box left.
#[test]
#[ignore = "native: KVM, a delegated cgroup and explicit pinned inputs (HEX_NATIVE_FC_SMOKE)"]
fn native_firecracker_vmm_ending_before_readiness_is_refused_with_its_output() {
    let inputs = Inputs::load();
    let own = own_cgroup();
    let config = inputs.path("broken_config");
    let binary = hex_runtime::firecracker::Config::from_file(&config)
        .unwrap()
        .binary;
    let node = Node::start(&inputs, &own, "b", &config);
    let client = node.client();
    let id = BoxId::new("fc-smoke-broken").unwrap();
    let begun = Instant::now();
    let refusal = client
        .create_before(
            spec(&inputs, &config, &id, &["/probe", "--hold"]),
            Instant::now() + inputs.wait,
        )
        .unwrap_err();
    eprintln!("SMOKE refused after {:?}: {refusal:?}", begun.elapsed());
    let Refusal::Broken(why) = &refusal else {
        panic!("{refusal:?}")
    };
    let prefix = "Firecracker ended before guest readiness; its last output:\n";
    assert!(
        why.starts_with(prefix) && why.len() > prefix.len(),
        "{why:?}"
    );
    assert!(client.list().unwrap().is_empty(), "the node lists no box");
    assert!(
        state_dir_entries(&config).is_empty(),
        "{:?}",
        state_dir_entries(&config)
    );
    let config_text = hex_runtime::firecracker::Config::from_file(&config).unwrap();
    assert!(
        vmm(&binary, &config_text.state_dir).is_empty(),
        "no VMM names the backend's state"
    );
    drop(client);
    let state = node.state.clone();
    drop(node);
    assert!(records(&state, &id).is_empty(), "no record of the box");
}

/// The GPU run's input `field`, under `gpu` in HEX_NATIVE_FC_SMOKE.
fn gpu<'a>(inputs: &'a Inputs, field: &str) -> &'a Value {
    let value = &inputs.value["gpu"][field];
    assert!(!value.is_null(), "input gpu.{field}");
    value
}

fn gpu_path(inputs: &Inputs, field: &str) -> PathBuf {
    PathBuf::from(gpu(inputs, field).as_str().unwrap())
}

/// The GPU box's spec: the selected GPU, named by its catalog key, in a box of the witness's root, its admitted
/// charge from the backend's own cost.
fn gpu_spec(inputs: &Inputs, config: &Path, id: &BoxId) -> BoxSpec {
    let config = hex_runtime::firecracker::Config::from_file(config).unwrap();
    let cpu_milli = gpu(inputs, "cpu_milli").as_u64().unwrap();
    let mem_bytes = gpu(inputs, "memory_bytes").as_u64().unwrap();
    let allocation = config
        .cost
        .allocate(CapacityResources {
            cpu_milli,
            mem_bytes,
            disk_bytes: 0,
        })
        .unwrap();
    BoxSpec {
        id: id.clone(),
        bounds: Bounds {
            cpu_max_milli: Some(u32::try_from(cpu_milli).unwrap()),
            memory_max: Some(mem_bytes),
            ..Bounds::default()
        },
        class: ClassSpec::Firecracker(Box::new(OciSpec {
            rootfs: Rootfs::Dir(gpu_path(inputs, "rootfs")),
            init: Program::of(&["/probe", "--hold"]),
            mounts: Vec::new(),
            hostname: None,
            network: Network::Private,
            hosts: Vec::new(),
        })),
        owner: None,
        devices: vec![DeviceId {
            kind: DeviceKind::Gpu,
            key: gpu(inputs, "key").as_u64().unwrap(),
        }],
        allocation: Some(allocation),
        route: None,
    }
}

/// Runs `program` in box `id`: its exit, stdout and stderr. Its output's end comes when it ends; `wait` bounds its
/// exit after that.
fn exec_to_end(
    client: &Client,
    id: &BoxId,
    program: Program,
    wait: Duration,
) -> (Option<Exit>, String, String) {
    let (mut out, out_writer) = std::io::pipe().unwrap();
    let (mut err, err_writer) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            id,
            program,
            Stdio {
                stdout: Some(out_writer),
                stderr: Some(err_writer),
                ..Stdio::null()
            },
        )
        .unwrap();
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        err.read_to_string(&mut text).unwrap();
        text
    });
    let mut said = String::new();
    out.read_to_string(&mut said).unwrap();
    let exit = proc.wait_for(wait).unwrap();
    (exit, said, errors.join().unwrap())
}

/// The Vulkan witness with `args`, its environment the run's (`gpu.env`) beside `PATH`.
fn witness(inputs: &Inputs, args: &[&str]) -> Program {
    let path = gpu(inputs, "witness").as_str().unwrap();
    let mut all = vec![path];
    all.extend_from_slice(args);
    let mut program = Program::of(&all);
    for pair in gpu(inputs, "env").as_array().unwrap() {
        let pair = pair.as_array().unwrap();
        program.env.push((
            pair[0].as_str().unwrap().to_owned(),
            pair[1].as_str().unwrap().to_owned(),
        ));
    }
    program
}

/// The processes this user can see holding VFIO group `group`'s file.
fn vfio_holders(group: u64) -> Vec<u32> {
    let target = PathBuf::from(format!("/dev/vfio/{group}"));
    let mut holders = Vec::new();
    for entry in fs::read_dir("/proc").unwrap() {
        let entry = entry.unwrap();
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        if fds
            .flatten()
            .any(|fd| fs::read_link(fd.path()).is_ok_and(|link| link == target))
        {
            holders.push(pid);
        }
    }
    holders
}

/// Opens VFIO group `group`'s file and closes it: a group is one holder's at a time (another open is EBUSY), so it
/// opens only while no process, of any user, holds its device. Nothing is done to the device.
fn vfio_group_opens(group: u64) -> std::io::Result<()> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/dev/vfio/{group}"))
        .map(drop)
}

/// The host driver of PCI function `sbdf`.
/// Whether the host's kernel lists no reset that clears PCI function `sbdf`, alone in IOMMU group `group`, at its
/// VM's end (docs/design/firecracker-gpu-host.md): no method of its own, and a bus reset, if listed, beside a device
/// on its bus that its VM isn't given. 0000:79:00.0 on 00aad8da lists `bus` beside its audio, crypto and USB functions.
fn single_use(sbdf: &str, group: u64) -> bool {
    let methods = fs::read_to_string(
        Path::new("/sys/bus/pci/devices")
            .join(sbdf)
            .join("reset_method"),
    )
    .unwrap_or_default();
    let listed = |names: &[&str]| methods.split_whitespace().any(|m| names.contains(&m));
    if listed(&["device_specific", "acpi", "flr", "af_flr", "pm"]) {
        return false;
    }
    let names = |dir: &str| -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect()
    };
    let given = names(&format!("/sys/kernel/iommu_groups/{group}/devices"));
    let bus = sbdf.rsplit_once(':').unwrap().0;
    let foreign = names("/sys/bus/pci/devices").iter().any(|name| {
        name.rsplit_once(':').is_some_and(|(on, _)| on == bus) && !given.contains(name)
    });
    !listed(&["bus", "cxl_bus"]) || foreign
}

/// The mark of `sbdf`, a function no reset clears, that the backend leaves once a VM was given it: in the node's
/// runtime directory (hex-node's `oci_root`), the one every node of its user on the host shares, `/run/hex-node` for
/// root, else `$XDG_RUNTIME_DIR/hex-node`. A rootless node with no `$XDG_RUNTIME_DIR` keeps it in its state's `oci`,
/// read by no other node, and gives no such function to a VM.
fn mark(state: &Path, sbdf: &str) -> PathBuf {
    let runtime = if rustix::process::geteuid().is_root() {
        PathBuf::from("/run/hex-node")
    } else {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .map_or_else(|| state.join("oci"), |dir| dir.join("hex-node"))
    };
    runtime.join("vfio-single-use").join(sbdf)
}

/// This boot of the host, as the kernel names it.
fn this_boot() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap()
        .trim()
        .to_owned()
}

fn driver(sbdf: &str) -> String {
    fs::read_link(format!("/sys/bus/pci/devices/{sbdf}/driver"))
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// The GPU VM's RAM as the run gives it: its box's memory, the least its VM holds.
fn gpu_ram(inputs: &Inputs) -> u64 {
    gpu(inputs, "memory_bytes").as_u64().unwrap()
}

/// AC-T7.4.1 (L1's memlock repro; the lead's ruling of 2026-11-11): a VFIO VM's VMM maps all its guest's RAM for DMA,
/// pinning it against its RLIMIT_MEMLOCK, and the GPU run r690's step a, under a hard limit below the VM's RAM, ended
/// in the VMM's `IommuDmaMap(Error(12))`, ENOMEM. Do: under a hard RLIMIT_MEMLOCK below the GPU VM's RAM, which this
/// test reads and requires (the run lowers it with `ulimit -l` in its own shell; the node inherits it), with no
/// CAP_IPC_LOCK for its VMM, create the GPU box. Expect its create refused by type before its VMM is launched, naming
/// the bytes of its guest's RAM, the soft and the hard limit and CAP_IPC_LOCK, and the GPU untouched: no box, no VM
/// state, no VMM, no holder of its VFIO group, which opens.
#[test]
#[ignore = "native GPU: KVM, a VFIO-bound GPU, a delegated cgroup and pinned inputs (HEX_NATIVE_FC_SMOKE gpu)"]
fn native_firecracker_gpu_vm_under_a_memlock_below_its_ram_is_refused_with_its_dma_error() {
    let inputs = Inputs::load();
    let limit = rustix::process::getrlimit(rustix::process::Resource::Memlock);
    let memlock = limit.maximum;
    let ram = gpu_ram(&inputs);
    assert!(
        memlock.is_some_and(|locked| locked < ram),
        "the hard RLIMIT_MEMLOCK {memlock:?} must be below the VM's RAM ({ram} bytes): lower it with `ulimit -l`"
    );
    let own = own_cgroup();
    let config = gpu_path(&inputs, "config");
    let binary = hex_runtime::firecracker::Config::from_file(&config)
        .unwrap()
        .binary;
    let group = gpu(&inputs, "group").as_u64().unwrap();
    assert!(vfio_holders(group).is_empty(), "the GPU is free before");
    let catalog = gpu(&inputs, "catalog").as_str().unwrap();
    let _reaper = Reaper::of(&config, inputs.wait);
    let node = Node::start_with(&inputs, &own, "gpu-memlock", &config, Some(catalog));
    let client = node.client();
    let id = BoxId::new("fc-gpu-memlock").unwrap();
    let begun = Instant::now();
    let refusal = client
        .create_before(
            gpu_spec(&inputs, &config, &id),
            Instant::now() + inputs.wait,
        )
        .unwrap_err();
    eprintln!(
        "GPU memlock {memlock:?} below RAM {ram}: refused after {:?}: {refusal:?}",
        begun.elapsed()
    );
    let Refusal::Platform { why, .. } = &refusal else {
        panic!("refused before its VMM, by its memlock: {refusal:?}")
    };
    let shown =
        |bound: Option<u64>| bound.map_or_else(|| "unlimited".to_owned(), |b| b.to_string());
    for named in [
        "bytes of its guest's RAM".to_owned(),
        format!("soft {}", shown(limit.current)),
        format!("hard {}", shown(limit.maximum)),
        "CAP_IPC_LOCK".to_owned(),
    ] {
        assert!(why.contains(&named), "{named}: {why}");
    }
    assert!(client.list().unwrap().is_empty(), "the node lists no box");
    assert!(
        state_dir_entries(&config).is_empty(),
        "{:?}",
        state_dir_entries(&config)
    );
    let state_dir = hex_runtime::firecracker::Config::from_file(&config)
        .unwrap()
        .state_dir;
    assert!(vmm(&binary, &state_dir).is_empty(), "no VMM is left");
    assert!(vfio_holders(group).is_empty(), "no process holds the GPU");
    vfio_group_opens(group).unwrap();
    drop(client);
    let state = node.state.clone();
    drop(node);
    assert!(records(&state, &id).is_empty(), "no record of the box");
    eprintln!("GPU memlock: returned, group {group} unheld and openable, no record");
}

/// AC-T7.4.1: the GPU run. Do, under an unlimited RLIMIT_MEMLOCK, through the real node and its `firecracker` class:
/// boot a VM with the selected GPU (the catalog's VFIO function) passed through; list the guest's PCI functions;
/// take the GPU's Vulkan identity in the box (the witness's inventory) and compute on it (its compute mode, checked
/// against its oracle); end the node by its PID (SIGKILL), its VM left running, and start another on the same state;
/// compute again through it; delete the box. Expect: one VMM, the only holder of the GPU's VFIO group, its boot words
/// the owner's and one GPU's, its passthrough the selected function; one GPU function in the guest and one Vulkan
/// device in the box, which computes with no mismatch on hardware; the restarted node's box adopted, the same VMM,
/// still computing; and after the delete no box, VM state, VMM, record or holder: the group opens, the function still
/// on vfio-pci.
#[test]
#[ignore = "native GPU: KVM, a VFIO-bound GPU, a delegated cgroup and pinned inputs (HEX_NATIVE_FC_SMOKE gpu)"]
fn native_firecracker_gpu_box_computes_recovers_and_returns_its_gpu() {
    let inputs = Inputs::load();
    let memlock = rustix::process::getrlimit(rustix::process::Resource::Memlock).current;
    let ram = gpu_ram(&inputs);
    assert!(
        memlock.is_none_or(|locked| locked >= ram),
        "RLIMIT_MEMLOCK {memlock:?} holds the VM's RAM ({ram} bytes)"
    );
    let own = own_cgroup();
    let config = gpu_path(&inputs, "config");
    let binary = hex_runtime::firecracker::Config::from_file(&config)
        .unwrap()
        .binary;
    let group = gpu(&inputs, "group").as_u64().unwrap();
    let host_sbdf = gpu(&inputs, "host_sbdf").as_str().unwrap();
    assert!(vfio_holders(group).is_empty(), "the GPU is free before");
    vfio_group_opens(group).unwrap();
    let catalog = gpu(&inputs, "catalog").as_str().unwrap();
    let _reaper = Reaper::of(&config, inputs.wait);
    let node = Node::start_with(&inputs, &own, "gpu", &config, Some(catalog));
    let single = single_use(host_sbdf, group);
    let spent = fs::read_to_string(mark(&node.state, host_sbdf))
        .is_ok_and(|boot| boot.trim() == this_boot());
    assert!(
        !(single && spent),
        "{host_sbdf} has no reset that clears it, and a VM had it on this boot already: reboot the host first"
    );
    let client = node.client();
    let id = BoxId::new("fc-gpu").unwrap();
    let spec = gpu_spec(&inputs, &config, &id);

    let begun = Instant::now();
    let info = client
        .create_before(spec.clone(), Instant::now() + inputs.wait)
        .unwrap();
    eprintln!(
        "GPU create (boot with the GPU to readiness, guest box made) {:?}",
        begun.elapsed()
    );
    if client.inspect(&id).unwrap().state == BoxState::Created {
        client.start(&id).unwrap();
    }
    let dir = vm_dir(&config);
    let running = vmm(&binary, &dir);
    assert_eq!(running.len(), 1, "one live VMM for the box: {running:?}");
    let vm: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("config.json")).unwrap()).unwrap();
    let boot = vm["boot-source"]["boot_args"].as_str().unwrap();
    let passthrough = &vm["device-passthrough"];
    eprintln!(
        "GPU VMM pid {} start {}; boot_args {boot:?}; device-passthrough {passthrough}",
        running[0].0, running[0].1
    );
    assert!(
        boot.contains(&format!("hex.gpu.box={id}")) && boot.contains("hex.gpu.pci=1"),
        "{boot}"
    );
    assert_eq!(passthrough.as_array().unwrap().len(), 1, "{passthrough}");
    assert_eq!(passthrough[0]["sbdf"], host_sbdf);
    assert_eq!(
        vfio_holders(group),
        vec![running[0].0],
        "the VMM alone holds the GPU"
    );

    let (exit, said, errors) =
        exec_to_end(&client, &id, Program::of(&["/probe", "gpu"]), inputs.wait);
    eprintln!("GPU probe: {exit:?} {said:?} {errors:?}");
    assert_eq!(exit, Some(Exit::Code(0)));
    let pci = said.lines().nth(1).unwrap();
    assert!(pci.ends_with(" pci-gpu-functions 1"), "{pci}");

    // The witness's inputs, each a fact or its protocol's, none picked:
    // - max-items: the GPUs the box was given, all the physical devices it may see (its own arrays, a
    //   device's extensions and queue families, it sizes by the driver's counts);
    // - shader bytes: the shader's own size, in the root the box was staged from;
    // - rounds: the oracle's period: it rotates a 32-bit word by 7 a round, coprime to 32, so 32 rounds
    //   carry every bit through every position (compute.comp, the witness's `oracle`);
    // - words: one page of them, the least a host-visible buffer maps (the guest's page, x86_64's as the host's);
    // - the fence's wait: the run's own bound on each step; the challenge: the run's, fresh each run.
    let max_items = spec.devices.len().to_string();
    let shader = gpu(&inputs, "shader").as_str().unwrap();
    let shader_bytes =
        fs::metadata(gpu_path(&inputs, "rootfs").join(shader.trim_start_matches('/')))
            .unwrap()
            .len()
            .to_string();
    let rounds = u32::BITS.to_string();
    let words = (rustix::param::page_size() / std::mem::size_of::<u32>()).to_string();
    let wait_ns = inputs.wait.as_nanos().to_string();
    let challenge = gpu(&inputs, "challenge").as_u64().unwrap().to_string();
    eprintln!(
        "GPU witness inputs: max-items {max_items} shader-bytes {shader_bytes} rounds {rounds} words {words} wait-ns {wait_ns} challenge {challenge}"
    );
    let (exit, said, errors) = exec_to_end(
        &client,
        &id,
        witness(&inputs, &["--mode", "inventory", "--max-items", &max_items]),
        inputs.wait,
    );
    eprintln!("GPU inventory: {exit:?} {said:?} {errors:?}");
    assert_eq!(exit, Some(Exit::Code(0)));
    let inventory: Value = serde_json::from_str(said.trim()).unwrap();
    let devices = inventory["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1, "the box sees the selected GPU alone");
    let uuid = devices[0]["uuid"].as_str().unwrap().to_owned();

    let compute = |client: &Client, when: &str| {
        let args = [
            "--mode",
            "compute",
            "--uuid",
            &uuid,
            "--max-items",
            &max_items,
            "--shader",
            shader,
            "--max-shader-bytes",
            &shader_bytes,
            "--words",
            &words,
            "--rounds",
            &rounds,
            "--challenge",
            &challenge,
            "--wait-ns",
            &wait_ns,
        ];
        let begun = Instant::now();
        let (exit, said, errors) = exec_to_end(client, &id, witness(&inputs, &args), inputs.wait);
        eprintln!(
            "GPU compute ({when}) {:?}: {exit:?} {said:?} {errors:?}",
            begun.elapsed()
        );
        assert_eq!(exit, Some(Exit::Code(0)));
        let done: Value = serde_json::from_str(said.lines().last().unwrap()).unwrap();
        assert_eq!(done["uuid"], uuid.as_str());
        assert_eq!(done["mismatches"], 0);
        assert_eq!(done["hardware"], true);
        assert_eq!(done["dispatch_completed"], true);
    };
    compute(&client, "first");

    drop(client);
    let begun = Instant::now();
    let node = node.restart(&inputs, &own, "gpu", &config);
    let client = node.client();
    let adopted = client.inspect(&id).unwrap();
    eprintln!(
        "GPU node restarted, ready after {:?}: {adopted:?}",
        begun.elapsed()
    );
    assert!(adopted.adopted, "the restarted node took its VM back");
    assert_eq!(adopted.allocation, info.allocation);
    assert_eq!(vmm(&binary, &dir), running, "the same VMM, never restarted");
    assert_eq!(vfio_holders(group), vec![running[0].0]);
    compute(&client, "after the node's restart");

    let begun = Instant::now();
    client.delete(&id).unwrap();
    eprintln!("GPU delete {:?}", begun.elapsed());
    assert!(client.list().unwrap().is_empty(), "the node lists no box");
    let (pid, start) = running[0];
    assert!(!alive(pid, start), "the VMM {pid} is gone");
    assert!(
        vmm(&binary, &dir).is_empty(),
        "no VMM names the VM's directory"
    );
    assert!(!dir.exists(), "the VM's state is gone");
    assert!(vfio_holders(group).is_empty(), "no process holds the GPU");
    vfio_group_opens(group).unwrap();
    assert_eq!(driver(host_sbdf), "vfio-pci");
    let state_dir = hex_runtime::firecracker::Config::from_file(&config)
        .unwrap()
        .state_dir;
    assert!(
        state_dir_entries(&config).is_empty(),
        "{:?}",
        state_dir_entries(&config)
    );
    if single {
        // Its one VM of this boot has ended: marked so, and the next on this boot, any node's, refused before its VMM.
        assert_eq!(
            fs::read_to_string(mark(&node.state, host_sbdf))
                .unwrap()
                .trim(),
            this_boot()
        );
        let second = BoxId::new("fc-gpu-second").unwrap();
        let begun = Instant::now();
        let refused = client
            .create_before(
                gpu_spec(&inputs, &config, &second),
                Instant::now() + inputs.wait,
            )
            .unwrap_err();
        eprintln!(
            "GPU second create on this boot refused after {:?}: {refused:?}",
            begun.elapsed()
        );
        assert!(
            matches!(&refused, Refusal::Platform { why, .. }
                if ["no reset method", "the host must reboot", host_sbdf].iter().all(|named| why.contains(named))),
            "{refused:?}"
        );
        assert!(client.list().unwrap().is_empty(), "the node lists no box");
        assert!(vmm(&binary, &state_dir).is_empty(), "no VMM was started");
        assert!(vfio_holders(group).is_empty(), "no process holds the GPU");
        assert!(state_dir_entries(&config).is_empty());
    }
    drop(client);
    let state = node.state.clone();
    drop(node);
    assert!(records(&state, &id).is_empty(), "no record of the box");
    eprintln!(
        "GPU returned: group {group} unheld and openable, {host_sbdf} on vfio-pci, no record"
    );
}

/// AC-T7.4.1: a failed test leaves no VMM (r761 left one holding the GPU's group after a panic), nor its VM's
/// directory (r985's reaper ended the VMM and left it in the backend's state). Do: start a stand-in VMM, a copy of
/// `/bin/sh` at a run's binary path whose command names its VM's config file in the run's state, as a VMM's does,
/// and drop a reaper for that binary and state. Expect the stand-in ended, by its own PID and start time, and its
/// VM's directory removed, and nothing else touched: a second stand-in naming another state still runs, its VM's
/// directory kept.
#[test]
fn a_reaper_ends_the_vmm_its_failed_test_left() {
    use std::os::unix::process::ExitStatusExt;
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("firecracker");
    fs::copy("/bin/sh", &binary).unwrap();
    let state = dir.path().join("fc-state");
    let other = dir.path().join("other-state");
    let start = |vm: &Path| {
        fs::create_dir_all(vm).unwrap();
        Command::new(&binary)
            .args([
                "-c",
                "while :; do read -r _ || exit 0; done",
                "--config-file",
                &vm.join("config.json").display().to_string(),
            ])
            .stdin(HostStdio::piped())
            .spawn()
            .unwrap()
    };
    let mut left = start(&state.join("vm"));
    let mut kept = start(&other.join("vm"));
    let found = |dir: &Path| vmm(&binary, dir);
    // The stand-ins are found once their exec is done, as the reaper finds a VMM.
    let until = Instant::now() + Duration::from_secs(30);
    while (found(&state).is_empty() || found(&other).is_empty()) && Instant::now() < until {
        std::thread::yield_now();
    }
    let (pid, began) = found(&state)[0];
    let (kept_pid, kept_began) = found(&other)[0];
    drop(Reaper {
        binary: binary.clone(),
        dir: state.clone(),
        wait: Duration::from_secs(30),
    });
    assert!(!running(pid, began), "the left VMM {pid} ended");
    assert!(!state.join("vm").exists(), "its VM's directory removed");
    assert!(
        running(kept_pid, kept_began),
        "another run's VMM {kept_pid} still runs"
    );
    assert!(other.join("vm").is_dir(), "another run's VM directory kept");
    assert!(left.wait().unwrap().signal().is_some());
    drop(kept.stdin.take());
    assert!(kept.wait().unwrap().success());
}
