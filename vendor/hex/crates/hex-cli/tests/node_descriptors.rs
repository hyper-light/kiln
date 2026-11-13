//! A node's descriptors (AC-T7.1.1): a node embedded in a process starts within that process's descriptor limit. Its
//! start raises the soft limit to the hard one, derives every bound that holds descriptors from what that leaves, so
//! its own fit by construction and what it admits past them is refused by type, and refuses to start only where the
//! hard limit can't hold the least node (one box and one connection). The root lab of 2026-11-11 ran a node in a
//! container whose soft limit was 2048 of a 2,097,152 hard one: it ran out at its namespaces' refill (EMFILE) and a
//! create failed, where `hex node` alone had raised its soft limit to its hard one.
//!
//! It needs a cgroup v2 subtree the test owns, with cpu, io, memory and pids delegated, to start nodes in. Where this
//! binary's own cgroup is delegated to it alone (HEX_DELEGATED's runner), it uses it; where it is shared, it re-runs
//! itself once in a transient `systemd-run --user --scope -p Delegate=yes`; where neither is possible it records the
//! skip rather than pass falsely.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Program, Refusal, Runtime, Signal, Stdio};
use hex_node::Node;
use hex_node::netns::{FilesBound, open_descriptors};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

const TEST: &str = "a_node_starting_under_a_low_descriptor_limit_holds_its_own_or_refuses_typed";
/// Set in the copy of this binary re-run to start a node under a lowered descriptor limit: `soft` lowers the soft
/// limit alone; `hard` the hard one with it, to [`FIT`]; `least` both to just past what the copy holds already.
const MODE_ENV: &str = "HEX_NODE_DESCRIPTORS_MODE";
const PASS: &str = "low descriptor limit: the node's start held its own or refused";
/// The root lab's container's soft limit (the fedora image's), under which an embedded node ran out mid-refill.
const LOW: u64 = 2048;
/// A hard limit far under a node's own at its memory-derived bounds (hundreds of thousands on these hosts) and past the
/// least node's: its fitted budget is a few dozen programs, which the case runs to the end of.
const FIT: u64 = 256;
/// What the least case leaves past the descriptors the copy holds as it lowers its limit: room for the node's start
/// to open what it opens before it fits its bounds (its box roots, store, keeper and key), never for the least node,
/// one box and one connection, whose descriptors past those held are more (`hex_node::netns::fit`).
const LEAST_ROOM: u64 = 8;

/// AC-T7.1.1. Do: start a node in a process whose soft descriptor limit is 2048 and its hard one as given; in one whose
/// hard limit is 256, then run programs in it until one is refused; in one whose hard limit leaves less than the least
/// node needs. Expect: the first start raises the soft limit to the hard one; the second starts with bounds that fit
/// that limit and refuses the run past its budget by type, never EMFILE; the third start is refused by type, naming
/// the hard limit.
#[test]
fn a_node_starting_under_a_low_descriptor_limit_holds_its_own_or_refuses_typed() {
    if let Some(mode) = std::env::var_os(MODE_ENV) {
        start_under_a_low_limit(&mode.to_string_lossy());
        return;
    }
    let cache = cache_dir();
    let parent = match prepare_cgroup(&cache) {
        Ok(own) => own,
        Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() && have_systemd() => {
            let status = Command::new("systemd-run")
                .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
                .arg(std::env::current_exe().expect("test binary path"))
                .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env("HEX_DELEGATED", "1")
                .status()
                .expect("run the test in a delegated scope");
            assert!(status.success(), "the delegated re-run failed ({direct})");
            return;
        }
        // HEX_DELEGATED promised this binary a cgroup delegated to it: a preparation that fails there fails the test,
        // never a skip that would hide the breakage.
        Err(direct) if std::env::var_os("HEX_DELEGATED").is_some() => {
            panic!("its cgroup isn't delegated to it ({direct})")
        }
        Err(direct) => {
            hex_testkit::skip(
                TEST,
                "cgroup v2 delegation",
                &format!("own cgroup isn't ours to subdivide ({direct}); run on a cgroup-v2 host"),
            )
            .expect("recording the skip");
            return;
        }
    };
    for mode in ["soft", "hard", "least"] {
        // Each copy alone in a fresh cgroup of its own, written before exec, as its node subdivides it.
        let own = parent.join(format!("hex-nofile-{mode}-{}", std::process::id()));
        std::fs::create_dir(&own).expect("a fresh cgroup for the copy");
        let output = Command::new("/bin/sh")
            .args([
                "-e",
                "-c",
                r#"printf '%s' "$$" > "$1/cgroup.procs"; shift; exec "$@""#,
                "hex-nofile",
            ])
            .arg(&own)
            .arg(std::env::current_exe().expect("test binary path"))
            .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(MODE_ENV, mode)
            .output()
            .expect("run the copy under a lowered descriptor limit");
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        eprint!("{log}");
        assert!(output.status.success(), "{mode}: the copy failed");
        assert!(
            log.lines().any(|line| line == PASS),
            "{mode}: the copy did not start its node"
        );
    }
}

/// The copy's half: lower this process's descriptor limit as `mode` says, start a node in this copy's own cgroup, and
/// hold it to what its start must do.
fn start_under_a_low_limit(mode: &str) {
    let given = getrlimit(Resource::Nofile);
    let hard = given.maximum.expect("a finite hard descriptor limit");
    let held = open_descriptors(&given).expect("this process's descriptors");
    let (soft, maximum) = match mode {
        "soft" => (LOW.min(hard), hard),
        "hard" => (FIT.min(hard), FIT.min(hard)),
        "least" => {
            let least = held + LEAST_ROOM;
            (least.min(hard), least.min(hard))
        }
        other => panic!("unknown mode {other}"),
    };
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(soft),
            maximum: Some(maximum),
        },
    )
    .expect("lowering the descriptor limit");
    let cache = cache_dir();
    let own = prepare_cgroup(&cache).expect("this copy's own delegated cgroup");
    let state = tempfile::Builder::new()
        .prefix("nofile-")
        .tempdir_in(&cache)
        .expect("state dir");
    let opened = Node::open(&own.join("hex-boxes-nofile"), state.path(), state.path());
    match mode {
        "soft" => {
            let node = opened.expect("the node's start");
            // Its own at the bounds its start fitted, read after it.
            let needed = FilesBound::needed(node.pools(), held);
            let limit = getrlimit(Resource::Nofile);
            assert_eq!(
                limit.current,
                Some(hard),
                "the start raised its soft limit to the hard one"
            );
            assert!(
                limit.current.is_some_and(|soft| soft >= needed.own),
                "{limit:?} holds the node's own {} descriptors",
                needed.own
            );
        }
        "hard" => {
            let node = opened.expect("a node whose bounds fit its hard limit starts");
            runs_past_the_budget_are_refused(&node, maximum);
        }
        _ => match opened {
            Err(Refusal::Platform { why, .. }) => assert!(
                why.contains(&format!("hard limit of {maximum}")),
                "the refusal names the hard limit: {why}"
            ),
            Err(other) => panic!("refused, but not for its descriptors: {other}"),
            Ok(_) => panic!(
                "a node started under a hard limit of {maximum}, past which it can hold no box"
            ),
        },
    }
    eprintln!("{PASS}");
}

/// Runs a program in a box of its own, each held, until the node refuses one: the refusal is its budget's, by type,
/// at the budget its start fitted to the limit, and the descriptors held stay within the limit.
fn runs_past_the_budget_are_refused(node: &Node, hard: u64) {
    let budget = node.sizes().inits;
    assert!(
        budget < hex_runtime::MAX_BOXES,
        "the budget fits the limit: {budget}"
    );
    let program = Program::of(&[env!("CARGO_BIN_EXE_hex"), "selftest-box", "wait"]);
    let mut ran = Vec::new();
    let refused = loop {
        assert!(
            ran.len() <= budget,
            "{} runs past a budget of {budget}",
            ran.len()
        );
        let id = BoxId::new(&format!("nofile-{}", ran.len())).unwrap();
        let spec = BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Process,
            owner: None,
            band: 0,
        };
        match node.run(spec, program.clone(), Stdio::null()) {
            Ok(proc) => ran.push((id, proc)),
            Err(refusal) => break refusal,
        }
    };
    let held = open_descriptors(&getrlimit(Resource::Nofile)).expect("the node's descriptors");
    let count = ran.len();
    eprintln!(
        "hard limit {hard}: {count} runs within a budget of {budget}, {held} descriptors held at the refusal ({refused})"
    );
    for (id, proc) in ran {
        let _ = node.signal(&id, Signal::KILL);
        drop(proc);
        // A run's box goes once its program ends (decision 7): the delete removes it, waits for that removal and
        // answers as it went, or finds it gone already.
        match node.delete(&id) {
            Ok(()) | Err(Refusal::NoSuchBox(_)) => {}
            Err(e) => panic!("deleting a run's box: {e:?}"),
        }
    }
    assert!(
        matches!(&refused, Refusal::Exhausted { bound, .. } if *bound == budget),
        "the run past the budget is refused by its budget, by type: {refused}"
    );
    assert_eq!(count, budget, "every run within the budget ran");
    assert!(
        held < hard,
        "{held} descriptors held within the hard limit of {hard}"
    );
}

fn have_systemd() -> bool {
    Command::new("systemd-run")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// This process's own cgroup, delegated to it and prepared once for every test in this binary
/// (`hex_testkit::cgroup::delegated`: the process and every process of its own moved into a leaf), or why it isn't
/// ours to subdivide. The probe root's opening proves every controller a box uses is delegated here: `BoxRoot::open`
/// refuses by name otherwise (HEX-INST-1).
fn prepare_cgroup(disk: &Path) -> Result<PathBuf, String> {
    hex_testkit::cgroup::delegated(|own| {
        let probe = hex_runtime::linux::cgroup::BoxRoot::open(
            &own.join("hex-probe"),
            hex_runtime::linux::disks::under(disk).unwrap_or_default(),
        )
        .map(|_| ())
        .map_err(|e| e.to_string());
        let _ = std::fs::remove_dir(own.join("hex-probe"));
        probe
    })
}

fn cache_dir() -> PathBuf {
    let cache = PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/nofile");
    std::fs::create_dir_all(&cache).expect("cache dir");
    cache
}
