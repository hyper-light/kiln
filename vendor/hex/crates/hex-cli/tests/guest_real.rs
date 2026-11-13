//! HEX-GUEST-19 (native plan step 5, docs/design/shards-guest.md §6): the `oci` class's conformance suite,
//! every case, through a real Shards guest booted by `shards_api::Shards` from this host, the path a macOS
//! or Windows node takes: `hex_runtime::guest::WithGuest` boots one guest, stages each box's root into it
//! and forwards every call to the Linux node agent inside. "Boot the guest, run every case, expect each to
//! pass and the guest booted once" (its boots counted at the `shards_api::Guests` the node is given).
//!
//! It needs a guest, so it runs only where one is given, and says SKIP and why elsewhere:
//! - `HEX_GUEST_KERNEL`: the guest's kernel (Shards' pinned 6.18; an arm64 `Image` or x86_64 `vmlinux`);
//! - `HEX_GUEST_INITRD`: its initramfs, whose `/init` is a static Linux `hex` (as PID 1, `hex` is the guest's init);
//! - `SHARDS_BIN`: the `shards` binary (on macOS the signed build, where it is installed);
//! - `HEX_GUEST_LINUX_HEX`: a static Linux `hex` for the guest's architecture, put at `/hex` in each
//!   box's root (a tarball, as §4a's roots may be on every host) so the suite's programs (`hex selftest-box`) run there;
//! - optionally `HEX_GUEST_CPUS` and `HEX_GUEST_MEMORY_MIB` (else the host's CPUs and Shards' least plus twice `PEAK_MIB`) and
//!   `HEX_GUEST_SHARES` (else the OS's default roots).
//!
//! Run: `cargo test -p hex-cli --test guest_real -- --nocapture` with those set.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use hex_api::conformance::{self, Act, Fixture};
use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Network, OciSpec, Program, Rootfs, Runtime};
use hex_runtime::guest::{GuestConfig, GuestRuntime, shares};

/// The guest VM process's peak resident set under this test's workload, in MiB, the most of three runs
/// (HEX-GUEST-19 on adas-mac-studio.local-2, aarch64 HVF, 2026-11-06; bench/native/guest/peak-rss/RECORD.md).
const PEAK_MIB: u64 = 284;

/// `oci` boxes whose root holds the Linux `hex` at `/hex` alone, their programs its selftest acts.
struct OciBoxes(PathBuf);

fn act(act: &Act) -> Program {
    let mut args = vec!["/hex".to_owned(), "selftest-box".to_owned()];
    args.extend(match act {
        Act::Say(words) => vec!["say".to_owned(), (*words).to_owned()],
        Act::Fail(code) => vec!["fail".to_owned(), code.to_string()],
        Act::Wait => vec!["wait".to_owned()],
        Act::Hog(bytes) => vec!["hog".to_owned(), bytes.to_string()],
        Act::Spawn(n) => vec!["spawn".to_owned(), n.to_string()],
        Act::Echo => vec!["echo".to_owned()],
    });
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Program::of(&args)
}

impl OciBoxes {
    fn box_spec(&self, id: BoxId, bounds: Bounds, init: &Act) -> BoxSpec {
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id,
            bounds,
            class: ClassSpec::Oci(Box::new(OciSpec {
                rootfs: Rootfs::Dir(self.0.clone()),
                init: act(init),
                mounts: Vec::new(),
                hostname: None,
                network: Network::Private,
                hosts: Vec::new(),
            })),
            owner: None,
            band: 0,
        }
    }
}

impl Fixture for OciBoxes {
    fn spec(&self, id: BoxId, bounds: Bounds) -> BoxSpec {
        self.box_spec(id, bounds, &Act::Wait)
    }

    fn init(&self, id: BoxId, act: &Act) -> BoxSpec {
        self.box_spec(id, Bounds::default(), act)
    }

    fn program(&self, a: &Act) -> Program {
        act(a)
    }
}

fn given(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// The boxes' root as a tarball in `dir`, its one file `/hex` mode 0755: staged from its own entries on
/// every host, as a Windows host must (docs/design/shards-guest.md §4a).
fn rootfs(dir: &std::path::Path) -> PathBuf {
    let rootfs = dir.join("rootfs.tar");
    let hex = std::fs::read(given("HEX_GUEST_LINUX_HEX").unwrap()).unwrap();
    let mut tarball = tar::Builder::new(std::fs::File::create(&rootfs).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o755);
    header.set_size(u64::try_from(hex.len()).unwrap());
    tarball
        .append_data(&mut header, "hex", hex.as_slice())
        .unwrap();
    tarball.into_inner().unwrap();
    rootfs
}

/// Whether a Shards guest is given, else why not, for a SKIP to say.
fn guest_given() -> Result<(), String> {
    let missing: Vec<&str> = [
        "HEX_GUEST_KERNEL",
        "HEX_GUEST_INITRD",
        "SHARDS_BIN",
        "HEX_GUEST_LINUX_HEX",
    ]
    .into_iter()
    .filter(|n| std::env::var_os(n).is_none())
    .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "no Shards guest given ({} unset)",
            missing.join(", ")
        ))
    }
}

#[test]
fn the_oci_class_holds_its_contract_through_a_real_shards_guest() {
    if let Err(why) = guest_given() {
        eprintln!("SKIP: {why}");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let rootfs = rootfs(root.path());
    let number =
        |name: &str, default: u64| std::env::var(name).map_or(default, |v| v.parse().unwrap());
    let config = GuestConfig {
        binary: given("SHARDS_BIN").unwrap(),
        kernel: given("HEX_GUEST_KERNEL").unwrap(),
        initrd: given("HEX_GUEST_INITRD").unwrap(),
        // Unset, the host's CPUs (`host_cpus`): the fastest run, as a vCPU costs nothing while it
        // idles.
        vcpus: std::env::var("HEX_GUEST_CPUS").map_or_else(|_| host_cpus(), |v| v.parse().unwrap()),
        gpu: false,
        // Unset, Shards' least guest and twice this workload's measured peak (PEAK_MIB): the margin
        // C's guest_tree takes, stated as a factor, never below what was measured.
        memory_mib: number(
            "HEX_GUEST_MEMORY_MIB",
            shards_api::min_memory_mib(std::env::consts::ARCH) + 2 * PEAK_MIB,
        ),
        shares: shares::from_env().unwrap(),
        staged: hex_client::guest::stage::STAGED.to_owned(),
    };
    let guests = Arc::new(Counted {
        shards: shards_api::Shards::new(),
        boots: AtomicUsize::new(0),
    });
    let node = GuestRuntime::new(guests.clone(), config);
    let failures = conformance::run(&node, &OciBoxes(rootfs));
    assert!(
        failures.is_empty(),
        "oci boxes through a real Shards guest: {failures:#?}"
    );
    assert!(node.list().unwrap().is_empty(), "a box is left");
    // One guest per node (docs/design/shards-guest.md §1): every box of the suite went to one guest. A
    // boot not ready within its learned bound is retired and booted again (HEX-GUEST-34), which a loaded
    // host may ask of the first: those are counted and said, never a guest that served a box.
    let (boots, retired) = (guests.boots.load(Ordering::SeqCst), node.retired());
    eprintln!("guest boots {boots}, retired before ready {retired}");
    assert_eq!(
        boots - usize::try_from(retired).unwrap(),
        1,
        "the node's boxes went to more than one guest ({boots} boots, {retired} retired)"
    );
}

/// Shards, counting each guest it is asked to boot.
struct Counted {
    shards: shards_api::Shards,
    boots: AtomicUsize,
}

impl shards_api::Guests for Counted {
    fn probe_gpu(
        &self,
        spec: &shards_api::GuestSpec,
        patience: Duration,
    ) -> Result<shards_api::GpuProbe, shards_api::Refusal> {
        self.shards.probe_gpu(spec, patience)
    }

    fn backend(&self) -> Result<shards_api::Backend, shards_api::Refusal> {
        self.shards.backend()
    }

    fn boot(
        &self,
        spec: &shards_api::GuestSpec,
    ) -> Result<Box<dyn shards_api::Guest>, shards_api::Refusal> {
        self.boots.fetch_add(1, Ordering::SeqCst);
        self.shards.boot(spec)
    }

    fn attach(
        &self,
        identity: &shards_api::Identity,
        spec: &shards_api::GuestSpec,
    ) -> Result<Box<dyn shards_api::Guest>, shards_api::Refusal> {
        self.shards.attach(identity, spec)
    }
}

/// A `hex node` of this build on macOS whose `oci` class is a Shards guest it boots when it starts
/// (HEX-GUEST-39), its state in `dir`; SIGKILLed when dropped, as a crash ends it.
#[cfg(target_os = "macos")]
struct GuestNode {
    child: std::process::Child,
    socket: PathBuf,
}

#[cfg(target_os = "macos")]
impl GuestNode {
    fn start(dir: &std::path::Path) -> Self {
        use std::io::BufRead;
        let socket = dir.join("node.sock");
        let memory = shards_api::min_memory_mib(std::env::consts::ARCH) + 2 * PEAK_MIB;
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hex"))
            .arg("node")
            .arg("--socket")
            .arg(&socket)
            .arg("--state")
            .arg(dir.join("state"))
            .env("HEX_GUEST_CPUS", host_cpus().to_string())
            .env("HEX_GUEST_MEMORY_MIB", memory.to_string())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("start hex node");
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(
            line.trim(),
            format!("ready {}", socket.display()),
            "hex node"
        );
        Self { child, socket }
    }

    fn client(&self) -> hex_client::Client {
        hex_client::Client::connect(&self.socket).expect("connect to hex node")
    }

    /// The node's descendants now, by the process tree (`ps`'s pid and parent), not by command line: its
    /// guest's VM processes.
    fn descendants(&self) -> Vec<u32> {
        let out = std::process::Command::new("/bin/ps")
            .args(["-Ao", "pid=,ppid="])
            .output()
            .unwrap();
        let pairs: Vec<(u32, u32)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                Some((w.next()?.parse().ok()?, w.next()?.parse().ok()?))
            })
            .collect();
        let mut found = vec![self.child.id()];
        let mut at = 0;
        while at < found.len() {
            let parent = found[at];
            found.extend(
                pairs
                    .iter()
                    .filter(|(_, pp)| *pp == parent)
                    .map(|(p, _)| *p),
            );
            at += 1;
        }
        found.remove(0);
        found
    }

    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

#[cfg(target_os = "macos")]
impl Drop for GuestNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(target_os = "macos")]
fn alive(pid: u32) -> bool {
    hex_runtime::macos::sys::rusage(i32::try_from(pid).unwrap())
        .unwrap()
        .is_some_and(|r| !r.ended)
}

/// Native plan step 8 (HEX-RT-1..13 through the socket): the oci class's suite through a live `hex node` on
/// macOS whose guest it booted itself: every case passes, as in process (the test above).
#[cfg(target_os = "macos")]
#[test]
fn the_oci_class_holds_its_contract_through_the_nodes_socket() {
    if let Err(why) = guest_given() {
        eprintln!("SKIP: {why}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let rootfs = rootfs(dir.path());
    let node = GuestNode::start(dir.path());
    let client = node.client();
    assert_eq!(
        client.classes(),
        [hex_api::Class::Process, hex_api::Class::Oci]
    );
    let failures = conformance::run(&client, &OciBoxes(rootfs));
    assert!(
        failures.is_empty(),
        "oci boxes through hex node's socket: {failures:#?}"
    );
    assert!(client.list().unwrap().is_empty(), "a box is left");
}

/// Native plan step 8 (T-T7.1.2): `hex node` killed (SIGKILL) with an oci box running in its guest, then
/// restarted on the same state: the guest outlived the node (its VM is a process of its own), so the node
/// takes it back (`shards_api::Guests::attach` of the identity it recorded), and with it the box, still
/// running, a program still exec'd in it; no second guest is booted, and no VM is left orphaned: none of
/// the first guest's VM processes runs beside a guest the restarted node booted instead.
#[cfg(target_os = "macos")]
#[test]
fn an_oci_box_in_the_guest_outlives_its_node_and_is_taken_back() {
    use hex_api::{BoxState, Stdio};
    use std::io::Read;
    if let Err(why) = guest_given() {
        eprintln!("SKIP: {why}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let fixture = OciBoxes(rootfs(dir.path()));
    let id = BoxId::new("outlives").unwrap();
    let node = GuestNode::start(dir.path());
    let vm = node.descendants();
    assert!(!vm.is_empty(), "the node booted no guest");
    let client = node.client();
    client
        .create(fixture.spec(id.clone(), Bounds::default()))
        .unwrap();
    client.start(&id).unwrap();
    assert_eq!(client.inspect(&id).unwrap().state, BoxState::Running);
    drop(client);
    node.kill();
    assert!(
        vm.iter().any(|p| alive(*p)),
        "the guest ended with its node"
    );
    let node = GuestNode::start(dir.path());
    let booted_again = node.descendants();
    let first_alive: Vec<u32> = vm.iter().copied().filter(|p| alive(*p)).collect();
    let client = node.client();
    let back = client.inspect(&id);
    let ran = back.as_ref().ok().map(|_| {
        let (mut r, w) = std::io::pipe().unwrap();
        let p = client.exec(
            &id,
            act(&Act::Say("still")),
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        );
        let mut out = String::new();
        let _ = r.read_to_string(&mut out);
        (p.is_ok(), out)
    });
    let _ = client.delete(&id);
    node.kill();
    // The first guest's VM, ended by its recorded pids whatever the test found.
    for p in vm.iter().chain(&booted_again) {
        if alive(*p) {
            let _ = std::process::Command::new("/bin/kill")
                .args(["-9", &p.to_string()])
                .status();
        }
    }
    let back = back.expect("the box in the guest is taken back");
    assert!(back.adopted, "{back:?}");
    assert_eq!(back.state, BoxState::Running, "{back:?}");
    assert_eq!(
        ran,
        Some((true, "still\n".to_owned())),
        "an exec in the taken-back box"
    );
    assert!(
        booted_again.is_empty() || first_alive.is_empty(),
        "an orphan VM: the first guest's processes {first_alive:?} ran on beside the guest the restarted \
         node booted ({booted_again:?})"
    );
    assert!(
        booted_again.is_empty(),
        "the restarted node booted another guest ({booted_again:?}) rather than take back the one that \
         outlived it"
    );
}

/// The host's CPUs, as std reads them: a guest here has every one, as a vCPU costs nothing while
/// it idles.
fn host_cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap())
}
