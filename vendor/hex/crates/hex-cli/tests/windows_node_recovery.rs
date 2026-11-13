//! T-T7.1.2 on Windows (native plan step 8): `hex node` killed (`TerminateProcess`) with boxes running,
//! empty and mid-delete, then restarted on the same state folder: each box its record says was made is
//! taken back (a running one with its member still running and watched, so a delete ends it), a half-deleted
//! one is cleaned up (no record, no member left), and nothing else is left.
//!
//! Mid-delete: a delete records the box as deleting before it ends anything, so a node killed past that
//! write leaves the record deleting and the box's members running; this test leaves exactly that (a box
//! made, a member started, the node killed, its record set to deleting), as recovery's own model holds a
//! crash at every write (`hex_core::recovery`). Mid-create is the store's and recovery's alone, the same
//! code on every OS (`tests/macos_node_recovery.rs` holds a node in that write; Windows has no FIFO to hold
//! it with).
#![cfg(windows)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hex_api::{Bounds, BoxId, BoxSpec, BoxState, ClassSpec, Program, Refusal, Runtime, Stdio};
use hex_core::record::{BoxRecord, Phase};

/// Longest a test waits for a fact it needs: a bound, so a node that never makes it true fails the test
/// instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(60);

/// A `hex node` of this build on `state`, ended by `TerminateProcess` when killed or dropped (a crash, as
/// the test means one).
struct Node {
    child: std::process::Child,
    socket: PathBuf,
}

impl Node {
    fn start(dir: &Path) -> Self {
        let socket = dir.join("node.sock");
        let mut child = Command::new(env!("CARGO_BIN_EXE_hex"))
            .arg("node")
            .arg("--socket")
            .arg(&socket)
            .arg("--state")
            .arg(dir.join("state"))
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("start hex node");
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), format!("ready {}", socket.display()));
        Self { child, socket }
    }

    fn client(&self) -> hex_client::Client {
        hex_client::Client::connect(&self.socket).expect("connect to hex node")
    }

    /// `TerminateProcess` (std's kill on Windows), and waited out.
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

fn spec(id: &BoxId) -> BoxSpec {
    BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    }
}

fn wait() -> Program {
    Program::of(&[env!("CARGO_BIN_EXE_hex"), "selftest-box", "wait"])
}

/// Whether process `pid` is still running, by asking `tasklist` (pure std): its output names the pid only
/// while it runs.
fn alive(pid: u32) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .expect("tasklist");
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

fn wait_for(what: &str, mut fact: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !fact() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::park_timeout(Duration::from_millis(20));
    }
}

fn record(dir: &Path, id: &BoxId) -> PathBuf {
    hex_node::store::runtime_record(&dir.join("state/@records"), id)
}

/// Stops the keeper a node of the test's state folder left running (`hex keep` outlives its node, which is
/// the point), so a test leaves none behind. It reads `state/keeper/keeper.owner` (the keeper's pid and
/// start time) and asks it to stop, as a node going down cleanly does. Declared first in a test, so it
/// drops after every `Node`.
struct Keepers(PathBuf);

impl Keepers {
    fn of(dir: &Path) -> Self {
        Self(dir.join("state").join("keeper"))
    }

    /// The keeper `keeper.owner` names: its pid and creation time.
    fn owner(&self) -> Option<(u32, u64)> {
        let text = std::fs::read_to_string(self.0.join("keeper.owner")).ok()?;
        let mut words = text.split_whitespace();
        Some((words.next()?.parse().ok()?, words.next()?.parse().ok()?))
    }
}

impl Drop for Keepers {
    fn drop(&mut self) {
        if let Some((pid, created)) = self.owner()
            && let Ok(keeper) =
                hex_runtime::windows::keeper::NodeKeeper::open(pid, created, &self.0)
        {
            keeper.stop();
        }
    }
}

/// Ends every process of the box's member left running if the test fails before its delete does: the
/// test's own cleanup, so a red run leaves nothing behind.
struct Leftover(u32);

impl Drop for Leftover {
    fn drop(&mut self) {
        if alive(self.0) {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.0.to_string(), "/T", "/F"])
                .output();
        }
    }
}

/// T-T7.1.2: do kill the node with a box made and running and one made and empty, then restart it on the
/// same state; expect the running box's member still running, both boxes read as taken back, and a delete
/// ending the member and leaving no record.
#[test]
fn boxes_made_are_taken_back_after_a_kill() {
    let dir = tempfile::Builder::new().prefix("rec-").tempdir().unwrap();
    let _keepers = Keepers::of(dir.path());
    let (running, empty) = (BoxId::new("running").unwrap(), BoxId::new("empty").unwrap());
    let node = Node::start(dir.path());
    let client = node.client();
    client.create(spec(&running)).unwrap();
    client.create(spec(&empty)).unwrap();
    let member = client.exec(&running, wait(), Stdio::null()).unwrap();
    let pid = member.pid();
    let _leftover = Leftover(pid);
    // Released, as a caller that keeps a box running past its own connection does.
    drop(member);
    drop(client);
    node.kill();
    assert!(alive(pid), "the box's member ended with the node");
    let node = Node::start(dir.path());
    let client = node.client();
    let back = client
        .inspect(&running)
        .expect("the running box is taken back");
    assert!(back.adopted, "{back:?}");
    assert_eq!(back.state, BoxState::Running, "{back:?}");
    let back = client.inspect(&empty).expect("the empty box is taken back");
    assert!(back.adopted, "{back:?}");
    client.delete(&running).unwrap();
    wait_for("the taken-back box's member to end with its delete", || {
        !alive(pid)
    });
    client.delete(&empty).unwrap();
    assert!(!record(dir.path(), &running).exists());
    assert!(!record(dir.path(), &empty).exists());
}

/// T-T7.1.2: do kill the node past the write that marks a box deleting (its record deleting, its member
/// running), then restart it; expect the member ended and the box and its record gone.
#[test]
fn a_box_killed_mid_delete_is_cleaned_up() {
    let dir = tempfile::Builder::new().prefix("rec-").tempdir().unwrap();
    let _keepers = Keepers::of(dir.path());
    let id = BoxId::new("mid-delete").unwrap();
    let node = Node::start(dir.path());
    let client = node.client();
    client.create(spec(&id)).unwrap();
    let member = client.exec(&id, wait(), Stdio::null()).unwrap();
    let pid = member.pid();
    let _leftover = Leftover(pid);
    drop(member);
    drop(client);
    node.kill();
    let path = record(dir.path(), &id);
    let mut r = BoxRecord::decode(&std::fs::read(&path).unwrap()).unwrap();
    r.phase = Phase::Deleting;
    std::fs::write(&path, r.encode().unwrap()).unwrap();
    assert!(alive(pid), "the box's member ended with the node");
    let node = Node::start(dir.path());
    let client = node.client();
    assert_eq!(client.inspect(&id), Err(Refusal::NoSuchBox(id.clone())));
    wait_for("the half-deleted box's member to be ended", || !alive(pid));
    assert!(!path.exists(), "the record was left");
}

/// T-T7.1.2 (native plan step 8): do start `hex node` with its stdout and stderr piped to the test, as a
/// supervisor reading its output starts it, and kill it (`TerminateProcess`); expect both to reach their end
/// with the node, though its keeper runs on to hold its boxes: the keeper holds none of its node's pipes, so
/// what reads the node's output isn't kept waiting by it (bench/install/jupiter.sh's tee waited on the five
/// keepers windows_node.rs's tests left, 2026-11-09).
#[test]
fn a_killed_nodes_output_ends_with_it_though_its_keeper_runs_on() {
    let dir = tempfile::Builder::new().prefix("out-").tempdir().unwrap();
    let keepers = Keepers::of(dir.path());
    let socket = dir.path().join("node.sock");
    let mut node = Command::new(env!("CARGO_BIN_EXE_hex"))
        .arg("node")
        .arg("--socket")
        .arg(&socket)
        .arg("--state")
        .arg(dir.path().join("state"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start hex node");
    let mut out = std::io::BufReader::new(node.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), format!("ready {}", socket.display()));
    let err = node.stderr.take().unwrap();
    let (keeper, _) = keepers.owner().expect("the node records its keeper");
    node.kill().unwrap();
    node.wait().unwrap();
    assert!(alive(keeper), "the node's keeper ended with it");
    let (ended, ends) = std::sync::mpsc::channel();
    let streams: [(&'static str, Box<dyn std::io::Read + Send>); 2] =
        [("stdout", Box::new(out)), ("stderr", Box::new(err))];
    for (name, mut stream) in streams {
        let ended = ended.clone();
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut stream, &mut std::io::sink());
            let _ = ended.send(name);
        });
    }
    let deadline = Instant::now() + PATIENCE;
    let mut open = vec!["stdout", "stderr"];
    while !open.is_empty() {
        match ends.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(name) => open.retain(|n| *n != name),
            Err(_) => panic!(
                "the node's {open:?} didn't end with it in {PATIENCE:?}: its keeper holds the pipe"
            ),
        }
    }
}
