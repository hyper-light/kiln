//! The keeper's machine (joint plan step 13): `hex node` serving its `process` class and `hex schedule serve` keeping the
//! machine's capacity view on its QUIC lane, each a process of its own, as production runs them; the scheduler's
//! enrollment begun by `hex node enroll` and finished with a CA's answer (`hex_quic::enroll::store`); Hex's own boxes
//! placed through the scheduler; and the keeper's books read back from its records once it has ended.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};

use hex_api::capacity::{Amounts, Budget, Held, Keeper, Records, Stop};
use hex_api::{Bounds, BoxId, BoxInfo, BoxSpec, Class, ClassSpec, Runtime};
use hex_core::schedule::{self as dto, Command as Ask};
use hex_node::scheduler::{Backend, CapacityHolder, CapacityLane, Config};
use hex_quic::enroll::{Issued, Request, store};

/// The `hex` binary of this build.
pub fn hex() -> &'static str {
    env!("CARGO_BIN_EXE_hex")
}

/// A process of the test's: the lines it said on stdout until its `ready`, its stderr kept as it comes; killed when
/// dropped, a Windows node with the keeper that outlives it by design.
pub struct Process {
    pub child: Child,
    pub lines: Vec<String>,
    said: Arc<Mutex<String>>,
    #[cfg(windows)]
    keeping: Option<PathBuf>,
}

impl Process {
    /// Starts `command`, and reads its stdout until it says `ready`. `keeping`: a Windows node's keeper folder.
    fn start(mut command: Command, keeping: Option<PathBuf>) -> Self {
        #[cfg(not(windows))]
        let _ = keeping;
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start a hex process");
        let said = Arc::new(Mutex::new(String::new()));
        let stderr = child.stderr.take().unwrap();
        let sink = Arc::clone(&said);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                sink.lock().unwrap().push_str(&line);
                line.clear();
            }
        });
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if out.read_line(&mut line).unwrap() == 0 {
                let ended = child.wait().unwrap();
                panic!(
                    "{command:?} ended {ended} having said {lines:?}: {}",
                    said.lock().unwrap()
                );
            }
            let line = line.trim_end().to_owned();
            let ready = line.starts_with("ready ");
            lines.push(line);
            if ready {
                break;
            }
        }
        // Nothing more comes on stdout; what it says on stderr is kept.
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut out, &mut std::io::sink());
        });
        Self {
            child,
            lines,
            said,
            #[cfg(windows)]
            keeping,
        }
    }

    pub fn said(&self) -> String {
        self.said.lock().unwrap().clone()
    }

    /// Ends it as `kill -9` does (TerminateProcess on Windows), and reaps it.
    pub fn kill(&mut self) -> ExitStatus {
        let _ = self.child.kill();
        self.child.wait().unwrap()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // The keeper its `keeper.owner` names is asked to stop, as a node going down cleanly does (windows_node.rs).
        #[cfg(windows)]
        if let Some(keeping) = &self.keeping
            && let Ok(text) = std::fs::read_to_string(keeping.join("keeper.owner"))
        {
            let mut words = text.split_whitespace();
            if let (Some(Ok(pid)), Some(Ok(created))) = (
                words.next().map(str::parse::<u32>),
                words.next().map(str::parse::<u64>),
            ) && let Ok(keeper) =
                hex_runtime::windows::keeper::NodeKeeper::open(pid, created, keeping)
            {
                keeper.stop();
            }
        }
    }
}

/// The scheduler's enrollment folder in the machine's folder.
pub fn keeper_credentials(dir: &Path) -> PathBuf {
    dir.join("keeper")
}

/// `hex node enroll --name <name>` in the machine's folder: the request for the CA, its key kept here alone.
pub fn enroll(dir: &Path, name: &str) -> Request {
    let credentials = keeper_credentials(dir);
    let begun = Command::new(hex())
        .args(["node", "enroll", "--name", name, "--credentials"])
        .arg(&credentials)
        .output()
        .expect("hex node enroll");
    assert!(
        begun.status.success(),
        "{}",
        String::from_utf8_lossy(&begun.stderr)
    );
    store::request(&credentials, Vec::new()).unwrap()
}

/// The CA's answer kept for the scheduler's enrollment.
pub fn keep(dir: &Path, issued: &Issued) {
    let credentials = keeper_credentials(dir);
    store::finish(&credentials, issued).unwrap();
    store::load(&credentials).unwrap();
}

/// `hex node` over the machine's folder (`n.sock`, `node/`), its boxes in a cgroup under `box_root` on Linux.
pub fn node(dir: &Path, box_root: Option<&Path>) -> Process {
    let mut command = Command::new(hex());
    command
        .arg("node")
        .arg("--socket")
        .arg(dir.join("n.sock"))
        .arg("--state")
        .arg(dir.join("node"));
    if let Some(root) = box_root {
        command.arg("--box-root").arg(root);
    }
    Process::start(command, Some(dir.join("node").join("keeper")))
}

/// The scheduler's configuration in the machine's folder: one node, its `oci` placement class the node's `process`
/// boxes, and the capacity lane bound at `bind` serving `holders` alone, with the enrollment kept here.
pub fn configure(dir: &Path, bind: SocketAddr, holders: &[&str]) -> PathBuf {
    let config = Config {
        cluster: "hex-capacity-view".into(),
        node: 0,
        state: dir.join("scheduler"),
        socket: dir.join("s.sock"),
        nodes: Vec::new(),
        models: Vec::new(),
        backends: vec![Backend {
            class: "oci".into(),
            socket: dir.join("n.sock"),
        }],
        peers: Vec::new(),
        capacity: Some(CapacityLane {
            bind,
            credentials: keeper_credentials(dir),
            holders: holders
                .iter()
                .map(|name| CapacityHolder {
                    name: (*name).to_owned(),
                })
                .collect(),
        }),
    };
    let path = dir.join("scheduler.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    path
}

/// `hex schedule serve` with the machine's configuration: the process and the address its lane bound.
pub fn scheduler(dir: &Path) -> (Process, SocketAddr) {
    let mut command = Command::new(hex());
    command
        .args(["schedule", "serve", "--config"])
        .arg(dir.join("scheduler.json"));
    let process = Process::start(command, None);
    let address = process
        .lines
        .iter()
        .find_map(|line| line.strip_prefix("capacity "))
        .unwrap_or_else(|| panic!("the scheduler said {:?}", process.lines))
        .parse()
        .unwrap();
    (process, address)
}

/// Whether the scheduler over the node in `dir` places a `process` box under its `oci` placement class, as [`place`]
/// asks, read as the scheduler reads them: the platform admits the class (`hex_sim::facts::classes`: Linux and
/// Windows, never a Mac), and the capacity the node reports for the class's form offers a `process` route. The form
/// is `oci` where the node runs containers and `process` where it doesn't (hex-node's `Runtimes::capacity_snapshot`);
/// a Linux node's `oci` form offers both routes, a Windows node's its containers alone.
pub fn places_process_boxes(dir: &Path) -> bool {
    use hex_sim::model::{Class as Placed, Classes, Virt};
    let here = hex_sim::facts::platform(std::env::consts::OS, std::env::consts::ARCH);
    let (os, arch) = here.unwrap();
    let oci = Classes::NONE.with(Placed::Oci);
    if !hex_sim::facts::classes(os, arch, Virt::default(), oci).has(Placed::Oci) {
        return false;
    }
    let node = hex_client::Client::connect(&dir.join("n.sock")).unwrap();
    let form = if node.classes().contains(&Class::Oci) {
        Class::Oci
    } else {
        Class::Process
    };
    let routes: Vec<Class> = node
        .capacity(form)
        .unwrap()
        .map(|offer| offer.routes.iter().map(|r| r.key.class).collect())
        .unwrap_or_default();
    eprintln!("the node's {form:?} form offers routes of {routes:?}");
    routes.contains(&Class::Process)
}

/// A box of Hex's own of `cpu_milli` and the least memory a box holds, placed through the scheduler, its program not
/// started: what it reserves is all it does. The scheduler's answer.
pub fn place(dir: &Path, id: &str, cpu_milli: u64) -> dto::Reply {
    let spec = BoxSpec {
        id: BoxId::new(id).unwrap(),
        bounds: Bounds {
            cpu_max_milli: Some(u32::try_from(cpu_milli).unwrap()),
            memory_max: Some(hex_api::MIN_MEMORY),
            ..Bounds::default()
        },
        class: ClassSpec::Process,
        owner: None,
        devices: Vec::new(),
        allocation: None,
        route: None,
        band: 0,
    };
    let demand = format!("cpus={cpu_milli}m,mem={},class:oci", hex_api::MIN_MEMORY);
    hex_client::Client::connect(&dir.join("s.sock"))
        .unwrap()
        .schedule(Ask::Submit {
            demand,
            payload: dto::Payload { spec, start: false },
        })
        .unwrap()
}

/// The node's boxes, as its own socket lists them.
pub fn boxes(dir: &Path) -> Vec<BoxInfo> {
    hex_client::Client::connect(&dir.join("n.sock"))
        .unwrap()
        .list()
        .unwrap()
}

/// Every box the node holds deleted through its own socket.
pub fn delete_boxes(dir: &Path) {
    let node = hex_client::Client::connect(&dir.join("n.sock")).unwrap();
    for listed in node.list().unwrap() {
        node.delete(&listed.id).unwrap();
    }
    assert!(node.list().unwrap().is_empty());
}

/// The keeper's records, copied: the keeper reads its books from them as it boots, here without writing to its folder.
struct Copied(BTreeMap<String, String>);

impl Records for Copied {
    fn put(&mut self, name: &str, value: &str) -> Result<(), Stop> {
        self.0.insert(name.to_owned(), value.to_owned());
        Ok(())
    }

    fn remove(&mut self, name: &str) -> Result<(), Stop> {
        self.0.remove(name);
        Ok(())
    }

    fn all(&mut self) -> Result<Vec<(String, String)>, Stop> {
        Ok(self.0.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }
}

/// Holder `holder`'s reservations as the keeper's records hold them, once the scheduler has ended: each record file
/// (`<name>.rec`, written whole: `hex_node::records`) read and the books booted from them as the keeper boots.
pub fn books(dir: &Path, holders: usize, holder: &str) -> Vec<Held> {
    let mut records = BTreeMap::new();
    for entry in std::fs::read_dir(dir.join("scheduler").join("capacity")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "rec") {
            let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
            records.insert(name, std::fs::read_to_string(&path).unwrap());
        }
    }
    let budget = Budget {
        holders,
        reservations: hex_core::capacity::most_held().unwrap(),
        name_bytes: hex_core::wire::MAX_MESSAGE,
    };
    let keeper = Keeper::boot(budget, &mut Copied(records)).unwrap();
    keeper.held_by(holder).collect()
}

/// What `held` reserves, summed.
pub fn sum(held: &[Held]) -> Amounts {
    held.iter()
        .fold(Amounts::ZERO, |sum, h| sum.checked_add(h.amount).unwrap())
}
