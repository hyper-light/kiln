//! What the Shards image class's real-node tests share (joint plan steps 12 and 15): their inputs, each by its own
//! variable; the cgroup each runs under; the node's Shards home, and a pull or a build into it; a `hex node` with the
//! Shards class in the waits census; the census's lines; and each VM's admission, as the node's own cost sizes it.
//! `image_real.rs` (the class's cases) and `image_grants_real.rs` (its network grants on booted VMs) take them.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio as HostStdio};
use std::sync::mpsc;
use std::time::Instant;

use hex_api::{Allocation, CapacityResources};

/// The variable `name`'s value, which the run needs: failing by name where it is unset.
pub fn input(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is unset (crates/hex-cli/tests/image_node/mod.rs)"))
}

pub fn number(name: &str) -> u64 {
    input(name)
        .parse()
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// This binary's own cgroup, made a parent: it moves into the leaf `leaf`, as native_firecracker_smoke does. Linux's
/// alone: a node elsewhere holds its boxes without cgroups.
#[cfg(target_os = "linux")]
pub fn own_cgroup(leaf: &str) -> PathBuf {
    let line = fs::read_to_string("/proc/self/cgroup").unwrap();
    let own = Path::new("/sys/fs/cgroup").join(line.trim().strip_prefix("0::/").unwrap());
    if own.file_name().is_some_and(|name| name == leaf) {
        return own.parent().unwrap().to_path_buf();
    }
    let at = own.join(leaf);
    fs::create_dir_all(&at).unwrap();
    fs::write(at.join("cgroup.procs"), std::process::id().to_string()).unwrap();
    own
}

/// The node's Shards home and cache (`hex-node`'s `shards_native_paths`: beside its state's records), whose store
/// its image VMs boot from.
pub fn shards_dirs(state: &Path) -> (PathBuf, PathBuf) {
    (
        state.join("shards-native-home"),
        state.join("shards-native-cache"),
    )
}

/// Runs `command` in the node's Shards home and cache to its end, its stdout the test's and its stderr echoed whole as it
/// ends; one that fails is refused naming its command, its exit and its stderr, so a pull or build says why in its own
/// words (HVF (a), ROOT's HVF run at c8b4ce6b, whose build named only its command).
fn in_home(command: &mut Command, home: &Path, cache: &Path) {
    let ran = command
        .env("SHARDS_HOME", home)
        .env("SHARDS_CACHE", cache)
        .stdin(HostStdio::null())
        .stdout(HostStdio::inherit())
        .stderr(HostStdio::piped())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&ran.stderr);
    eprint!("{said}");
    assert!(
        ran.status.success(),
        "{command:?}: {}; its stderr:\n{}",
        ran.status,
        said.trim_end()
    );
}

/// Pulls `image` by its digest into the node's Shards home, then ends the daemon the pull started there, so no
/// process of the pull's runs beside the node.
pub fn pull(shards: &Path, image: &str, state: &Path) {
    let (home, cache) = shards_dirs(state);
    let shards_in_home = |args: &[&str]| in_home(Command::new(shards).args(args), &home, &cache);
    let begun = Instant::now();
    shards_in_home(&["pull", image]);
    eprintln!("IMAGE pulled {image} in {:?}", begun.elapsed());
    shards_in_home(&["daemon", "stop"]);
}

/// Builds the Agentfile image in `context` into the node's Shards home (`work`'s state), tagged `tag`, as `shards build
/// image` builds one, then ends the daemon the build left there, so none of its processes runs beside the node;
/// returns the image pinned by the manifest digest its build says (`--metadata-file`'s `containerimage.digest`, kept
/// in `work` beside the tag's name), which Shards' store finds whatever it is named.
pub fn built(shards: &Path, context: &Path, work: &Path, tag: &str) -> String {
    let (home, cache) = shards_dirs(&work.join("state"));
    let metadata = work.join(format!("{}.build.json", tag.replace('/', "-")));
    let in_home = |command: &mut Command| in_home(command, &home, &cache);
    let begun = Instant::now();
    in_home(
        Command::new(shards)
            .args(["build", "image", "-t", tag, "--metadata-file"])
            .arg(&metadata)
            .arg(context),
    );
    in_home(Command::new(shards).args(["daemon", "stop"]));
    let said: serde_json::Value = serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
    let digest = said["containerimage.digest"]
        .as_str()
        .unwrap_or_else(|| panic!("{metadata:?} names no containerimage.digest: {said}"));
    let image = format!("{tag}@{digest}");
    eprintln!(
        "IMAGE built {image} from {context:?} in {:?}",
        begun.elapsed()
    );
    image
}

/// A `hex node` with the Shards class, in the waits census, ended by its own PID when dropped: its image VMs' run
/// hosts then see their channel close and end their VMs (D231's run ends with its owner).
pub struct Node {
    pub child: Child,
    pub socket: PathBuf,
}

impl Node {
    /// Starts the node, its boxes in the cgroup `boxes` where one is named (Linux's `--box-root`), else where its OS's
    /// node holds them, and waits for its own "ready" line while it lives: a node that ends first ends its stdout,
    /// which refuses the start with its stderr (the person's rule of 2026-11-13: a cold start waits while its party
    /// lives, with no deadline picked for it).
    pub fn start(work: &Path, boxes: Option<&Path>) -> Self {
        let socket = work.join("node.sock");
        let stderr = work.join("node.stderr");
        let begun = Instant::now();
        let mut node = Command::new(env!("CARGO_BIN_EXE_hex"));
        node.arg("node")
            .arg("--socket")
            .arg(&socket)
            .arg("--state")
            .arg(work.join("state"));
        if let Some(boxes) = boxes {
            node.arg("--box-root").arg(boxes);
        }
        let mut child = node
            .env("HEX_WAITS_CENSUS", work.join("census.txt"))
            .env("HEX_WAITS_CENSUS_MAX", input("HEX_WAITS_CENSUS_MAX"))
            .stdin(HostStdio::null())
            .stdout(HostStdio::piped())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        // The node's own "ready <socket>" line is the fact waited on. Its stdout is read for its whole life, every line
        // after the wait shown, so the node never writes to a pipe no one reads.
        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Err(mpsc::SendError(line)) = tx.send(line) {
                    eprintln!("node: {line}");
                }
            }
        });
        let want = format!("ready {}", socket.display());
        loop {
            match rx.recv() {
                Ok(line) if line == want => break,
                Ok(line) => eprintln!("node: {line}"),
                Err(mpsc::RecvError) => panic!(
                    "node not ready: its stdout ended; stderr: {}",
                    fs::read_to_string(&stderr).unwrap_or_default()
                ),
            }
        }
        eprintln!(
            "IMAGE node ready after {:?} (pid {})",
            begun.elapsed(),
            child.id()
        );
        Self { child, socket }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        let status = self.child.wait().unwrap();
        eprintln!("IMAGE node ended: {status}");
    }
}

/// Every answer the client's own waits record, as the census line it is, said as it comes.
pub fn step(name: &'static str, sample: hex_core::patience::Sample) {
    eprint!(
        "IMAGE-STEP {}",
        hex_core::patience::census_line(name, sample)
    );
}

/// Each image wait's answers in the node's census file, `name nanos` (a refusal `name !nanos`, a run-out
/// `name >nanos`): its count, fastest and slowest answer, the slowest the record's.
pub fn records(census: &Path) {
    let text = fs::read_to_string(census).unwrap();
    if let Some(full) = text.lines().find(|l| l.starts_with("census full")) {
        eprintln!("IMAGE-RECORD cut: {full}");
    }
    for wait in ["hex.image.boot", "hex.image.stats", "hex.image.stop"] {
        let answers: Vec<u64> = text
            .lines()
            .filter_map(|l| l.strip_prefix(wait)?.strip_prefix(' ')?.parse().ok())
            .collect();
        let other = text
            .lines()
            .filter(|l| {
                l.strip_prefix(wait)
                    .and_then(|rest| rest.strip_prefix(' '))
                    .is_some_and(|v| v.starts_with('!') || v.starts_with('>'))
            })
            .count();
        let (fastest, slowest) = (answers.iter().min(), answers.iter().max());
        eprintln!(
            "IMAGE-RECORD {wait} n={} fastest_ns={fastest:?} slowest_ns={slowest:?} refused_or_ran_out={other}",
            answers.len()
        );
    }
}

/// Each VM's admission for the run's workload (`HEX_IMAGE_CPU_MILLI`, `HEX_IMAGE_MEMORY_BYTES`), as the node's own
/// cost sizes a VM, from the same configuration the node reads.
pub fn allocation() -> Allocation {
    let cost = hex_runtime::guest::GuestConfig::from_env_with_native_inputs()
        .unwrap()
        .cost;
    cost.allocate(CapacityResources {
        cpu_milli: number("HEX_IMAGE_CPU_MILLI"),
        mem_bytes: number("HEX_IMAGE_MEMORY_BYTES"),
        disk_bytes: 0,
    })
    .unwrap()
}
