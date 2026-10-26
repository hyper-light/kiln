//! An acknowledged publish survives kill -9 of any node (kiln's CP-T2.2, step 4), three nodes as
//! three processes (`quiver-drill`): the leader, a follower, and the node the publisher publishes
//! through, each killed with SIGKILL (TerminateProcess on Windows) at every persistence point of
//! a run of publishes, the k-th frame its logs write and flush for k = 1, 2, … until the run ends
//! before it, then started again on what its directory holds.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros,
        clippy::disallowed_methods
    )
)]

mod cluster;

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cluster::drill::{Heard, check};

const TOPIC: &str = "drill";
const RUN: usize = 12;
const STUCK: Duration = Duration::from_secs(60);

/// A node's process: its handle, its input and output (lent to the publisher's thread while a
/// run goes), and the frames it last said it wrote.
struct Process {
    child: Child,
    talk: Option<Talk>,
    frames: Arc<AtomicU64>,
}

/// A node process's input and output: one line out, its answer in.
struct Talk {
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Talk {
    /// The next line, empty once the process ended.
    fn line(&mut self) -> String {
        let mut line = String::new();
        let _ = self.output.read_line(&mut line);
        line.trim_end().to_owned()
    }

    fn ask(&mut self, line: &str) -> String {
        if writeln!(self.input, "{line}")
            .and_then(|()| self.input.flush())
            .is_err()
        {
            return String::new();
        }
        self.line()
    }

    /// (leads, term, end) of the topic.
    fn figures(&mut self) -> (bool, u64, u64) {
        let answer = self.ask(&format!("figures {TOPIC}"));
        let words: Vec<&str> = answer.split(' ').collect();
        assert_eq!(words[0], "figures", "{answer}");
        (
            words[1] == "1",
            words[2].parse().unwrap(),
            words[3].parse().unwrap(),
        )
    }

    fn publish(&mut self, payload: &str) -> Result<u64, String> {
        let answer = self.ask(&format!("publish {TOPIC} {payload}"));
        if answer.is_empty() {
            return Err("the node's process ended".to_owned());
        }
        match answer.strip_prefix("acked ") {
            Some(offset) => Ok(offset.parse().unwrap()),
            None => Err(answer),
        }
    }

    fn log(&mut self) -> Vec<(u64, String)> {
        let mut out = Vec::new();
        let mut from = 0;
        loop {
            let head = self.ask(&format!("read {TOPIC} {from}"));
            let words: Vec<&str> = head.split(' ').collect();
            assert_eq!(words[0], "messages", "{head}");
            let count: usize = words[1].parse().unwrap();
            if count == 0 {
                return out;
            }
            for _ in 0..count {
                let line = self.line();
                let (offset, payload) = line.split_once(' ').unwrap();
                out.push((offset.parse().unwrap(), payload.to_owned()));
            }
            from = words[2].parse().unwrap();
        }
    }
}

struct Lab {
    dir: PathBuf,
    nodes: String,
    processes: Vec<Option<Process>>,
}

impl Lab {
    fn start() -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("processes")
            .join(format!("{}-{}", std::process::id(), cluster::next()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let nodes = (1..=3)
            .map(|i| {
                format!(
                    "{i}={}",
                    SocketAddr::from(([127, 0, 0, 1], cluster::free_port()))
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        // The cluster's identities, made once before its nodes start, as an operator makes them
        // and gives each node the same directory: three nodes making them at once race.
        focal_bus::pki::load_or_make(
            &dir.join("identities"),
            Some(&dir.join("identities.key")),
            3,
            3,
        )
        .unwrap();
        let mut lab = Self {
            dir,
            nodes,
            processes: (0..3).map(|_| None).collect(),
        };
        // Each says it's ready once it caught up with its catalog's group: a quorum of them up.
        for id in 1..=3 {
            lab.spawn(id);
        }
        for id in 1..=3 {
            lab.ready(id);
        }
        lab
    }

    fn ready(&mut self, id: u64) {
        let ready = self.talk(id).line();
        assert!(ready.starts_with("ready "), "node {id}: {ready:?}");
    }

    fn spawn(&mut self, id: u64) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_quiver-drill"))
            .args(["--id", &id.to_string(), "--nodes", &self.nodes])
            .arg("--data")
            .arg(self.dir.join(format!("node-{id}")))
            .arg("--identities")
            .arg(self.dir.join("identities"))
            .arg("--identities-key")
            .arg(self.dir.join("identities.key"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let frames = Arc::new(AtomicU64::new(0));
        let errors = BufReader::new(child.stderr.take().unwrap());
        std::thread::spawn({
            let frames = Arc::clone(&frames);
            move || {
                for line in errors.lines() {
                    let Ok(line) = line else { return };
                    match line.strip_prefix("frames ") {
                        Some(n) => frames.store(n.parse().unwrap(), Ordering::Release),
                        None => eprintln!("node {id}: {line}"),
                    }
                }
            }
        });
        let talk = Talk {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
        };
        self.processes[usize::try_from(id - 1).unwrap()] = Some(Process {
            child,
            talk: Some(talk),
            frames,
        });
    }

    fn process(&mut self, id: u64) -> &mut Process {
        self.processes[usize::try_from(id - 1).unwrap()]
            .as_mut()
            .unwrap()
    }

    fn talk(&mut self, id: u64) -> &mut Talk {
        self.process(id)
            .talk
            .as_mut()
            .expect("the node's talk is here")
    }

    /// SIGKILL: the process is gone at once, nothing of its own run on the way.
    fn kill(&mut self, id: u64) {
        if let Some(mut process) = self.processes[usize::try_from(id - 1).unwrap()].take() {
            process.child.kill().unwrap();
            process.child.wait().unwrap();
        }
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        for id in 1..=3 {
            self.kill(id);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let began = Instant::now();
    loop {
        if let Some(found) = f() {
            return found;
        }
        assert!(
            began.elapsed() < STUCK,
            "{what} didn't come within {STUCK:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[derive(Clone, Copy, Debug)]
enum Victim {
    Leader,
    Follower,
    PublishersOwn,
}

fn drill(victim: Victim, point: u64) -> Result<bool, String> {
    let mut lab = Lab::start();
    assert_eq!(lab.talk(1).ask(&format!("create {TOPIC}")), "ok");
    let leader = wait_for("a leader of the topic's group", || {
        (1..=3).find(|id| lab.talk(*id).figures().0)
    });
    let others: Vec<u64> = (1..=3).filter(|id| *id != leader).collect();
    let (victim_id, through) = match victim {
        Victim::Leader => (leader, others[0]),
        Victim::Follower => (others[0], others[1]),
        Victim::PublishersOwn => (leader, leader),
    };
    let fallback = (1..=3)
        .find(|id| *id != victim_id && *id != through)
        .unwrap();
    let frames = Arc::clone(&lab.process(victim_id).frames);
    let base = frames.load(Ordering::Acquire);
    // The publisher talks to its node's process (and to another's once that one is killed) on
    // a thread of its own; the kill comes from here.
    let mut first = lab.process(through).talk.take().unwrap();
    let mut second = lab.process(fallback).talk.take().unwrap();
    let publisher = std::thread::spawn(move || {
        let mut heard = Heard::default();
        let mut on_second = false;
        for i in 0..RUN {
            let payload = format!("{point}-a{i}");
            let talk = if on_second { &mut second } else { &mut first };
            match talk.publish(&payload) {
                Ok(offset) => {
                    heard.acked.insert(payload, offset);
                }
                // Refused, or its process killed under it: its fate is unknown.
                Err(_) => {
                    heard.unknown.insert(payload);
                    on_second = true;
                }
            }
        }
        (heard, first, second)
    });
    let mut ended_first = false;
    wait_for("the kill's frame or the run's end", || {
        if frames.load(Ordering::Acquire) >= base + point {
            return Some(());
        }
        if publisher.is_finished() {
            ended_first = true;
            return Some(());
        }
        None
    });
    lab.kill(victim_id);
    let (mut heard, first, second) = publisher.join().unwrap();
    if through != victim_id {
        lab.process(through).talk = Some(first);
    }
    lab.process(fallback).talk = Some(second);
    lab.spawn(victim_id);
    lab.ready(victim_id);
    for i in 0..3 {
        for id in 1..=3 {
            let payload = format!("{point}-b{id}-{i}");
            let offset = lab
                .talk(id)
                .publish(&payload)
                .map_err(|e| format!("a publish through node {id} after the kill: {e}"))?;
            heard.acked.insert(payload, offset);
        }
    }
    let end = heard.acked.values().max().copied().unwrap_or(0) + 1;
    wait_for("every node to hold every acknowledged publish", || {
        let ends: Vec<u64> = (1..=3).map(|id| lab.talk(id).figures().2).collect();
        (ends.iter().all(|e| *e == ends[0]) && ends[0] >= end).then_some(())
    });
    let logs: Vec<(u64, Vec<(u64, String)>)> = (1..=3).map(|id| (id, lab.talk(id).log())).collect();
    check(&heard, &logs).map_err(|why| format!("{victim:?} killed at frame {point}: {why}"))?;
    Ok(ended_first)
}

fn every_point(victim: Victim) -> Result<(), String> {
    let mut point = 1;
    loop {
        if drill(victim, point)? {
            assert!(point > 1, "the run ended before the victim's first frame");
            return Ok(());
        }
        point += 1;
    }
}

/// CP-T2.2 step 4: do SIGKILL the leader's process at every frame its logs write in a run of
/// publishes made through a follower, expect nothing acknowledged lost, nothing appended twice,
/// every node alike once it's back.
#[test]
fn cp_t2_2_an_acknowledged_publish_survives_kill_9_of_the_leader_at_every_persistence_point()
-> Result<(), String> {
    every_point(Victim::Leader)
}

/// CP-T2.2 step 4: the same, a follower's process killed.
#[test]
fn cp_t2_2_an_acknowledged_publish_survives_kill_9_of_a_follower_at_every_persistence_point()
-> Result<(), String> {
    every_point(Victim::Follower)
}

/// CP-T2.2 step 4: the same, the leader's process killed with the publisher's connection to it.
#[test]
fn cp_t2_2_an_acknowledged_publish_survives_kill_9_of_its_own_node_at_every_persistence_point()
-> Result<(), String> {
    every_point(Victim::PublishersOwn)
}
