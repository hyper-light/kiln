//! Node VM rows (joint plan step 39): kept apart from box and pool rows, read back as written, replaced whole, and
//! let go.

use std::path::Path;

use super::*;
use hex_api::{Allocation, Exit, NodeVmPhase};
use hex_core::record::RECORD_VERSION;

/// A node VM's admitted size, its cost's own allocation: one CPU and the least memory.
fn allocation() -> Allocation {
    let fixed = hex_api::CapacityResources {
        cpu_milli: 1000,
        mem_bytes: hex_api::MIN_MEMORY,
        disk_bytes: 0,
    };
    hex_api::RuntimeCost {
        id: [3; 32],
        floor: fixed,
        ceiling: fixed,
        quantum: hex_api::CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: hex_api::CapacityResources::default(),
        overhead: hex_api::CapacityResources::default(),
    }
    .allocate(hex_api::CapacityResources::default())
    .unwrap()
}

/// A process box's record, ready.
fn a_box(id: &str) -> hex_core::record::BoxRecord {
    let spec = hex_api::BoxSpec {
        route: None,
        devices: Vec::new(),
        id: hex_api::BoxId::new(id).unwrap(),
        bounds: hex_api::Bounds::default(),
        class: hex_api::ClassSpec::Process,
        owner: None,
        allocation: None,
        band: 0,
    };
    hex_core::record::BoxRecord::new(spec, hex_core::record::Phase::Ready, vec![])
}

/// `row` as its store keeps it, at `place` in its node's start order, which its first write was given: a fresh store's
/// first place is one past none.
fn placed(row: &NodeVmRecord, place: u64) -> NodeVmRecord {
    NodeVmRecord {
        started: Some(place),
        ..row.clone()
    }
}

fn record(run: u8, phase: NodeVmPhase) -> NodeVmRecord {
    NodeVmRecord {
        version: RECORD_VERSION,
        run: NodeRun::from_bytes([run; 16]),
        image:
            "hex-node:rev@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_owned(),
        allocation: allocation(),
        phase,
        diagnostics: None,
        exit: if phase == NodeVmPhase::Ended {
            Some(Exit::Code(0))
        } else {
            None
        },
        last_resort: None,
        started: None,
    }
}

/// ROOT's ruling of 2026-11-13: a node VM's record lies apart from every box's, so no box path sees it. Do: keep a
/// node VM's record beside a box's, read both kinds back, replace it, then let it go. Expect: the box rows list the box
/// alone and the pool rows nothing; the node VM rows the record as written, then as replaced, then none.
#[test]
fn a_node_vms_row_is_kept_apart_from_box_and_pool_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("state"), &dir.path().join("run")).unwrap();
    store.put(&a_box("a-box")).unwrap();
    let booting = record(7, NodeVmPhase::Booting);
    store.put_node_vm(&booting).unwrap();
    let boxes: Vec<_> = store.all().unwrap().into_keys().collect();
    assert_eq!(
        boxes,
        [hex_api::BoxId::new("a-box").unwrap()],
        "the box rows see no node VM"
    );
    assert!(
        store.all_pools().unwrap().is_empty(),
        "the pool rows see no node VM"
    );
    assert_eq!(
        store.node_vm(&booting.run).unwrap(),
        Some(placed(&booting, 1))
    );
    let ready = record(7, NodeVmPhase::Ready);
    store.put_node_vm(&ready).unwrap();
    assert_eq!(
        store.node_vm(&ready.run).unwrap(),
        Some(placed(&ready, 1)),
        "replaced whole"
    );
    store.remove_node_vm(&ready.run).unwrap();
    assert_eq!(store.node_vm(&ready.run).unwrap(), None);
}

/// Joint plan step 39 (ROOT, 2026-11-13: a node VM's row is its receipt's one source): do keep three node VMs' rows,
/// written out of their runs' order, beside a box's, and list them; then put a record file in their directory whose
/// name is no run's, and open the store again. Expect the three listed whole, in their runs' order, the box not among
/// them; then the next open refused for the file whose name is no run's.
#[test]
fn every_node_vms_row_is_listed_in_its_runs_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("state"), &dir.path().join("run")).unwrap();
    store.put(&a_box("a-box")).unwrap();
    let rows = [
        record(9, NodeVmPhase::Ended),
        record(2, NodeVmPhase::Booting),
        record(5, NodeVmPhase::Ready),
    ];
    for row in &rows {
        store.put_node_vm(row).unwrap();
    }
    assert_eq!(
        store.all_node_vms().unwrap(),
        [
            placed(&rows[1], 2),
            placed(&rows[2], 3),
            placed(&rows[0], 1)
        ]
    );
    // A file put behind the store's back is out of its contract: the next store's open reads it, and refuses.
    std::fs::write(store.node_vm_directory().join("a-box.json"), b"{}\n").unwrap();
    drop(store);
    assert!(matches!(
        Store::open(&dir.path().join("state"), &dir.path().join("run")),
        Err(Refusal::Invalid { .. })
    ));
}

/// Each wait of these cases on a fact of another process's: its word, its end, a row let go.
static FACT: hex_core::patience::Patience =
    hex_core::patience::Patience::measuring("test.hex_node.node_vm_rows");

/// The run of these cases' rows, `n` in each byte.
fn run(n: u8) -> NodeRun {
    NodeRun::from_bytes([n; 16])
}

/// Where a row of run `n` keeps its run host's diagnostics, in the test's directory, written as a run host would.
fn diagnosed(dir: &Path, n: u8, phase: NodeVmPhase) -> NodeVmRecord {
    let mut row = record(n, phase);
    let diagnostics = dir.join(format!("node-{n}.log"));
    std::fs::write(&diagnostics, b"its run host's words\n").unwrap();
    row.diagnostics = Some(diagnostics);
    row
}

fn store_in(dir: &Path) -> Store {
    Store::open(&dir.join("state"), &dir.join("run")).unwrap()
}

/// This test binary run as one of these cases' other processes, `role` naming it, in `dir`.
fn role(role: &str, dir: &Path) -> std::process::Command {
    let full = concat!(module_path!(), "::", "node_vm_role");
    let test = full.split_once("::").map_or(full, |(_, test)| test);
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env("HEX_NODE_VM_ROLE", role)
        .env("HEX_NODE_VM_DIR", dir);
    command
}

/// These cases' other processes, run alone where `HEX_NODE_VM_ROLE` names one (an ordinary run passes at once):
/// - `owner`, a node that runs node VM 7: it writes its row Booting, so it holds its lock, then ends;
/// - `ending owner`, the same, its row then Ready, saying so; once its stdin says `end`, it puts why its last resort
///   ends the VM into the row and writes it Ended, letting its lock go, then ends;
/// - `locking owner`, a process holding node VM 6's lock with no row and no store of the directory, as a dead owner's
///   lock may be held after its directory's is let go (Windows lets each go as its handle's close is done): it takes
///   the lock, says so, and ends when its stdin ends;
/// - `holding owner`, the same, then starts a child of its own that outlives it, which holds its lock file as its
///   stdin: on Unix a descriptor of its open file description, as a child forked without an exec would, and on Windows
///   a handle of its file object, as a child that inherited it would (the review's C-13); then reads its row back, as
///   a node does, says so, and ends when its stdin ends;
/// - `child`: the owner's child, which goes once its test's directory holds `done`, or no longer is, saying so
///   (`bye`).
#[test]
fn node_vm_role() {
    use std::io::{BufRead, Write};
    let (Some(role), Some(dir)) = (
        std::env::var_os("HEX_NODE_VM_ROLE"),
        std::env::var_os("HEX_NODE_VM_DIR"),
    ) else {
        return;
    };
    let dir = PathBuf::from(dir);
    if role == "child" {
        // Its pid, for a run that kills its test to stop it by.
        std::fs::write(dir.join("child.pid"), std::process::id().to_string()).unwrap();
        // A look: a file's appearing tells this fixture no event on every OS. Its party, its test, ends taking its
        // directory with it, which the fact takes for its word too.
        hex_testkit::waits::follow::until(
            &FACT,
            "its test's word to go",
            || hex_testkit::waits::follow::Party::Live,
            || dir.join("done").exists() || !dir.exists(),
        )
        .unwrap();
        let _ = std::fs::write(dir.join("bye"), b"");
        return;
    }
    if role == "locking owner" {
        let directory = dir.join("state").join(".node-vms");
        std::fs::create_dir_all(&directory).unwrap();
        let lock = open_lock(&directory, &run(6)).unwrap();
        assert!(try_own(&lock).unwrap());
        let mut out = std::io::stdout().lock();
        out.write_all(b"HEX_NODE_VM_OWNER_READY\n").unwrap();
        out.flush().unwrap();
        let mut line = String::new();
        while std::io::stdin().lock().read_line(&mut line).unwrap() > 0 {}
        drop(lock);
        return;
    }
    let store = store_in(&dir);
    let row = diagnosed(&dir, 7, NodeVmPhase::Booting);
    store.put_node_vm(&row).unwrap();
    if role == "owner" {
        return;
    }
    if role == "ending owner" {
        let ready = NodeVmRecord {
            phase: NodeVmPhase::Ready,
            ..row.clone()
        };
        store.put_node_vm(&ready).unwrap();
        let mut out = std::io::stdout().lock();
        out.write_all(b"HEX_NODE_VM_OWNER_READY\n").unwrap();
        out.flush().unwrap();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).unwrap();
        assert_eq!(line, "end\n");
        store
            .update_node_vm(&row.run, |row| {
                row.last_resort = Some(end_of(&row.run));
                true
            })
            .unwrap();
        store
            .put_node_vm(&NodeVmRecord {
                phase: NodeVmPhase::Ended,
                exit: Some(Exit::Signal(9)),
                ..ready
            })
            .unwrap();
        return;
    }
    let mut child = role_child(&dir);
    child.stdin(store.node_vm_lock(&row.run).unwrap());
    #[expect(
        clippy::zombie_processes,
        reason = "it outlives this process, as the case asks: it is never waited for here"
    )]
    let _outlives = child.spawn().unwrap();
    assert_eq!(store.node_vm(&row.run).unwrap(), Some(placed(&row, 1)));
    assert_eq!(store.all_node_vms().unwrap(), [placed(&row, 1)]);
    let mut out = std::io::stdout().lock();
    out.write_all(b"HEX_NODE_VM_OWNER_READY\n").unwrap();
    out.flush().unwrap();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
}

/// The owner's child, its stdio none but what its caller gives its stdin.
fn role_child(dir: &Path) -> std::process::Command {
    let mut child = role("child", dir);
    child
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    child
}

/// Joint plan step 39 (ROOT, 2026-11-13: a row from an earlier boot is ended at start): do write node VM 7's row
/// Ready as a node of an earlier boot of the host left it, its lock held by no process, beside its diagnostics, then
/// start a node. Expect the row let go, its owner ended, and its diagnostics with it.
#[test]
fn w39_a_node_vm_row_of_an_earlier_boot_is_let_go_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let row = diagnosed(dir.path(), 7, NodeVmPhase::Ready);
    left(dir.path(), &row);
    let store = store_in(dir.path());
    assert_eq!(store.node_vm(&row.run).unwrap(), Some(row.clone()));
    assert_eq!(
        store.settle_node_vms().unwrap().rows,
        [(row.run, NodeVmSettled::OwnerEnded)]
    );
    assert_eq!(store.node_vm(&row.run).unwrap(), None);
    assert!(!row.diagnostics.unwrap().exists());
}

/// Joint plan step 39 (ROOT, 2026-11-13: a row from an earlier process whose run is gone is ended at start): do run a
/// node that writes node VM 7's row Booting and ends, then start another. Expect the row let go, its owner ended (D231:
/// no microVM outlives its owner), with its lock file and its diagnostics.
#[test]
fn w39_a_node_vm_row_whose_owner_ended_is_let_go_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let ended = role("owner", dir.path()).status().unwrap();
    assert!(ended.success(), "{ended}");
    let store = store_in(dir.path());
    assert_eq!(
        store.node_vm(&run(7)).unwrap().map(|row| row.phase),
        Some(NodeVmPhase::Booting)
    );
    assert_eq!(
        store.settle_node_vms().unwrap().rows,
        [(run(7), NodeVmSettled::OwnerEnded)]
    );
    assert_eq!(store.node_vm(&run(7)).unwrap(), None);
    assert_eq!(
        std::fs::read_dir(store.node_vm_directory())
            .unwrap()
            .count(),
        0,
        "its lock file let go with its row"
    );
    assert!(!dir.path().join("node-7.log").exists());
}

/// Joint plan step 39 (ROOT, 2026-11-13: the lock is its owner's alone, never a child's), the store its directory's one
/// writer (batch 1114d's reads2; ROOT, 2026-11-14: an owner that runs holds its directory, so a start follows no row):
/// do run a node that writes node VM 7's row Booting, starts a child of its own that outlives it holding its lock file
/// (on Unix sharing its open file description, as a child forked without an exec would, and on Windows its file object,
/// as a child that inherited it would: the review's C-13), and reads its row back; open a store over its directory
/// while it runs; then end it and open one again. Expect the open refused while the owner runs, naming the directory;
/// then, the owner ended and its child still running, the next open's settle letting the row go, its owner ended, with
/// its lock file and its diagnostics.
#[test]
fn w39_a_live_owner_holds_its_directory_and_no_child_of_it_keeps_its_lock() {
    use std::io::{BufRead, Write};
    let dir = tempfile::tempdir().unwrap();
    eprintln!("node VM rows test directory: {}", dir.path().display());
    let mut owner = role("holding owner", dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = std::io::BufReader::new(owner.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            said.read_line(&mut line).unwrap(),
            0,
            "the owner ended unready"
        );
        if line.ends_with("HEX_NODE_VM_OWNER_READY\n") {
            break;
        }
    }
    let state = dir.path().join("state");
    let refused = Store::open(&state, &dir.path().join("run"));
    assert!(
        matches!(&refused, Err(Refusal::Broken(why)) if why.contains(&state.display().to_string())),
        "a store opened beside a live owner: {refused:?}"
    );
    let mut ends = owner.stdin.take().unwrap();
    ends.write_all(b"end\n").unwrap();
    drop(ends);
    assert!(owner.wait().unwrap().success());
    let store = opened_once_let_go(dir.path());
    assert_eq!(
        store.settle_node_vms().unwrap().rows,
        [(run(7), NodeVmSettled::OwnerEnded)]
    );
    assert_eq!(store.node_vm(&run(7)).unwrap(), None);
    assert!(
        !dir.path().join("bye").exists(),
        "the owner's child still runs, its lock file's description open where it shares it"
    );
    assert_eq!(
        std::fs::read_dir(store.node_vm_directory())
            .unwrap()
            .count(),
        0,
        "its lock file let go with its row"
    );
    assert!(!dir.path().join("node-7.log").exists());
    std::fs::write(dir.path().join("done"), b"").unwrap();
    // A look: the child, the owner's and no child of this test's, has no handle here; its going is the file it
    // writes as it goes.
    hex_testkit::waits::follow::until(
        &FACT,
        "the owner's child gone",
        || hex_testkit::waits::follow::Party::Live,
        || dir.path().join("bye").exists(),
    )
    .unwrap();
}

/// A store over `dir` once an owner that held it has ended: its hold is let go as its process ends, on Windows once its
/// handle's close is done. A look: the end tells no event here, the open's answer read.
fn opened_once_let_go(dir: &Path) -> Store {
    let opened = std::cell::RefCell::new(None);
    hex_testkit::waits::follow::until(
        &FACT,
        "the ended owner's directory let go",
        || hex_testkit::waits::follow::Party::Live,
        || match Store::open(&dir.join("state"), &dir.join("run")) {
            Ok(store) => {
                *opened.borrow_mut() = Some(store);
                true
            }
            Err(_) => false,
        },
    )
    .unwrap();
    opened.into_inner().unwrap()
}

/// The owner `role` names, started in `dir` and followed until it says it is ready: its stdin the test's to write, and
/// its stdout, which the test keeps open until the owner has ended, so the owner's last words find a reader.
fn owner_ready(
    role_named: &str,
    dir: &Path,
) -> (
    std::process::Child,
    std::io::BufReader<std::process::ChildStdout>,
) {
    use std::io::BufRead;
    #[expect(
        clippy::zombie_processes,
        reason = "its caller waits for it; one that ended unready has ended, the test with it"
    )]
    let mut owner = role(role_named, dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = std::io::BufReader::new(owner.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            said.read_line(&mut line).unwrap(),
            0,
            "the owner ended unready"
        );
        if line.ends_with("HEX_NODE_VM_OWNER_READY\n") {
            return (owner, said);
        }
    }
}

/// The review's C-11 (AC-T7.4.1; joint plan step 39, place 1), the store its directory's one writer (batch 1114d's
/// reads2; ROOT, 2026-11-14): an end its owner keeps as it ends is carried by the next node's start, which reads the
/// rows only once their owner has ended. Do run a node that writes node VM 7's row Ready; open a store over its
/// directory; then have the node put why its last resort ends the VM into the row, write it Ended and end; then open a
/// store again and settle its rows. Expect the first open refused while the owner runs, naming the directory; then the
/// row let go as Ended, its last-resort end read back, and the row gone.
#[test]
fn c11_a_row_its_owner_ended_is_let_go_as_ended_its_end_carried() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let (mut owner, said) = owner_ready("ending owner", dir.path());
    let state = dir.path().join("state");
    let refused = Store::open(&state, &dir.path().join("run"));
    assert!(
        matches!(&refused, Err(Refusal::Broken(why)) if why.contains(&state.display().to_string())),
        "a store opened beside a live owner: {refused:?}"
    );
    let mut ends = owner.stdin.take().unwrap();
    ends.write_all(b"end\n").unwrap();
    drop(ends);
    assert!(owner.wait().unwrap().success());
    drop(said);
    let store = opened_once_let_go(dir.path());
    let settled = store.settle_node_vms().unwrap();
    assert_eq!(
        (settled.rows, settled.ends),
        (vec![(run(7), NodeVmSettled::Ended)], vec![end_of(&run(7))])
    );
    assert_eq!(store.node_vm(&run(7)).unwrap(), None);
}

/// Joint plan step 39: do write node VM 7's row Ended, its end kept, then start a node. Expect the row let go, with its
/// diagnostics.
#[test]
fn w39_an_ended_node_vm_row_is_let_go_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    let row = diagnosed(dir.path(), 7, NodeVmPhase::Ended);
    store.put_node_vm(&row).unwrap();
    assert_eq!(
        store.settle_node_vms().unwrap().rows,
        [(row.run, NodeVmSettled::Ended)]
    );
    assert_eq!(store.node_vm(&row.run).unwrap(), None);
    assert!(!row.diagnostics.unwrap().exists());
}

/// Joint plan step 39: a node's start settles no row of its own. Do write node VM 7's row Booting in this node, read it
/// back, then settle the rows as a start does. Expect nothing settled, the row kept as written.
#[test]
fn w39_a_nodes_own_node_vm_row_is_never_settled() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    let row = diagnosed(dir.path(), 7, NodeVmPhase::Booting);
    store.put_node_vm(&row).unwrap();
    assert_eq!(store.node_vm(&row.run).unwrap(), Some(placed(&row, 1)));
    assert_eq!(store.settle_node_vms().unwrap().rows, []);
    assert_eq!(store.node_vm(&row.run).unwrap(), Some(placed(&row, 1)));
}

/// Why a last resort ended node VM `run`: 3 GiB held of a 2 GiB budget in band 0, its fleet past its budget.
fn end_of(run: &NodeRun) -> LearnedEnd {
    LearnedEnd {
        id: hex_api::LearnedHolder::Node(*run),
        held: 3 << 30,
        budget: 2 << 30,
        band: 0,
        fleet_held: 9 << 30,
        fleet_growth: 1 << 30,
        fleet_budget: 8 << 30,
        unmoved: Some(hex_api::Upstream::LiveMove),
    }
}

/// `row` as an earlier node process left it in `dir`'s rows, before a store over `dir` opens: its bytes in its place, its
/// lock held by no process.
fn left(dir: &Path, row: &NodeVmRecord) {
    let rows = dir.join("state").join(".node-vms");
    std::fs::create_dir_all(&rows).unwrap();
    std::fs::write(
        rows.join(format!("{}.json", name(&row.run).unwrap())),
        row.encode().unwrap(),
    )
    .unwrap();
}

/// Joint plan step 39, place 1 (C5, intent first; ROOT, 2026-11-13): a node VM row's last-resort end is its learner's
/// to write, and a phase write made from a read before it keeps it, so a reap racing the intent never drops it. Do
/// write node VM 7's row Booting, then Ready; put why the last resort is ending it into the row, as the learner does;
/// then write it Ended from the Ready record read before that end was. Expect the row Ended with its exit, its end kept.
#[test]
fn w39_a_node_vm_rows_phase_write_keeps_its_last_resort() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    store.put_node_vm(&record(7, NodeVmPhase::Booting)).unwrap();
    let ready = record(7, NodeVmPhase::Ready);
    store.put_node_vm(&ready).unwrap();
    let end = end_of(&run(7));
    assert!(
        store
            .update_node_vm(&run(7), |row| {
                row.last_resort = Some(end.clone());
                true
            })
            .unwrap()
    );
    let ended = NodeVmRecord {
        phase: NodeVmPhase::Ended,
        exit: Some(Exit::Signal(9)),
        ..ready
    };
    store.put_node_vm(&ended).unwrap();
    let kept = store.node_vm(&run(7)).unwrap().unwrap();
    assert_eq!(
        (kept.phase, kept.exit, kept.last_resort),
        (NodeVmPhase::Ended, Some(Exit::Signal(9)), Some(end))
    );
}

/// Joint plan step 39, place 1 (ROOT, 2026-11-13: an Ended row's last-resort end, and one whose owner ended, are read
/// back before the row goes): do leave, as an earlier node process would, node VM 4's row Ended and node VM 5's Ready,
/// no process holding its lock, each with why the last resort ended it, and node VM 6's Ended with none; then start a
/// node. Expect all three let go, and the two ends read back in their runs' order.
#[test]
fn w39_a_nodes_start_reads_back_each_let_go_rows_last_resort_end() {
    let dir = tempfile::tempdir().unwrap();
    let mut ended = record(4, NodeVmPhase::Ended);
    ended.last_resort = Some(end_of(&run(4)));
    let mut unowned = record(5, NodeVmPhase::Ready);
    unowned.last_resort = Some(end_of(&run(5)));
    for row in [&ended, &unowned, &record(6, NodeVmPhase::Ended)] {
        left(dir.path(), row);
    }
    let store = store_in(dir.path());
    let settled = store.settle_node_vms().unwrap();
    assert_eq!(
        settled.rows,
        [
            (run(4), NodeVmSettled::Ended),
            (run(5), NodeVmSettled::OwnerEnded),
            (run(6), NodeVmSettled::Ended),
        ]
    );
    assert_eq!(settled.ends, [end_of(&run(4)), end_of(&run(5))]);
    assert!(store.all_node_vms().unwrap().is_empty());
}

/// Joint plan step 39, place 3 (ROOT, 2026-11-13: a node VM's start place is its real start order among the host's
/// holders): do write node VM 7's row Booting, then Ready; then reopen the store as a restarted node does. Expect its row
/// to hold a place in the node's start order from its first write, the same place after its Ready write, and the next
/// place given past it, before and after the reopening.
#[test]
fn w39_a_node_vm_rows_place_in_its_nodes_start_order_is_given_at_its_first_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    store.put_node_vm(&record(7, NodeVmPhase::Booting)).unwrap();
    let place = store.node_vm(&run(7)).unwrap().unwrap().started;
    assert!(place.is_some(), "no place given at its first write");
    store.put_node_vm(&record(7, NodeVmPhase::Ready)).unwrap();
    assert_eq!(store.node_vm(&run(7)).unwrap().unwrap().started, place);
    assert!(Some(store.next_start().unwrap()) > place);
    drop(store);
    let reopened = store_in(dir.path());
    assert!(
        Some(reopened.next_start().unwrap()) > place,
        "the order runs on past the node VM's place"
    );
}

/// Joint plan step 39 (ROOT, 2026-11-13): one node VM serves a node's oci class, its one guest, so a node holds one node
/// VM Ready at a time, and a second is a damaged row, refused, typed. Do write node VM 1's row Booting, then Ready; node
/// VM 2's Booting, then Ready; then node VM 1's Ended, and node VM 2's Ready again. Expect node VM 2's first Ready refused
/// as a damaged row, naming the one guest, its row left Booting; then, node VM 1 ended, node VM 2 Ready.
#[test]
fn w39_a_node_holds_one_node_vm_ready_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    store.put_node_vm(&record(1, NodeVmPhase::Booting)).unwrap();
    store.put_node_vm(&record(1, NodeVmPhase::Ready)).unwrap();
    store.put_node_vm(&record(2, NodeVmPhase::Booting)).unwrap();
    let refused = store.put_node_vm(&record(2, NodeVmPhase::Ready));
    assert!(
        matches!(&refused, Err(Refusal::Invalid { why, .. }) if why.contains("one guest")),
        "{refused:?}"
    );
    assert_eq!(
        store.node_vm(&run(2)).unwrap().map(|row| row.phase),
        Some(NodeVmPhase::Booting)
    );
    store.put_node_vm(&record(1, NodeVmPhase::Ended)).unwrap();
    store.put_node_vm(&record(2, NodeVmPhase::Ready)).unwrap();
    assert_eq!(
        store.node_vm(&run(2)).unwrap().map(|row| row.phase),
        Some(NodeVmPhase::Ready)
    );
}

/// The node VM store's node_vm_unfollowed line (CLAUDE.md §10; ROOT, 2026-11-14): its run and its why JSON strings.
/// Do: form it for run 0xab's owner, refused in words holding a quote, a backslash and a newline. Expect: one line, the
/// run's name and the words JSON strings.
#[test]
fn an_unfollowed_node_vms_line_is_one_line_its_values_json_strings() {
    let why = hex_api::Refusal::Broken("a \"b\" \\ c\nd".into());
    assert_eq!(
        unfollowed_line(&run(0xab), &why),
        format!(
            "hex.guest event=node_vm_unfollowed run=\"{}\" why=\"the node agent's state is unusable: a \\\"b\\\" \\\\ \
             c\\nd\"",
            "ab".repeat(16)
        )
    );
}

/// The review's C-12 (AC-T7.4.1; joint plan step 39): a row write that fails leaves no temporary file, so none counts
/// against the rows' scan bound. Do publish node VM 7's row where its place is a folder holding a file, a place its
/// rename can't take. Expect the write refused and its temporary file gone.
#[test]
fn c12_a_row_write_that_fails_leaves_no_temporary_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    let directory = store.node_vm_directory();
    let (temporary, place) = scoped_paths(&directory, &name(&run(7)).unwrap()).unwrap();
    std::fs::create_dir_all(place.join("in-the-way")).unwrap();
    let bytes = record(7, NodeVmPhase::Booting).encode().unwrap();
    assert!(
        store
            .publish_durable(&directory, &temporary, &place, &bytes, || ())
            .is_err()
    );
    assert!(!temporary.exists(), "its temporary file left");
}

/// The review's C-12 (AC-T7.4.1; joint plan step 39): a node's start removes what a crash left in the node VM rows'
/// directory that no row names and no process holds, so none counts against the rows' scan bound for good: a row
/// write's temporary file, and a lock file with no row. Do leave node VM 4's temporary file and node VM 5's lock file
/// with no row, and run a node that holds node VM 6's lock with no row yet, as one does between its lock and its first
/// write; then start a node. Expect node VM 4's and 5's files removed and 6's lock file kept while its holder runs;
/// then, its holder ended, the next start removes it too.
#[test]
fn c12_a_starts_settle_removes_the_strays_a_crash_left() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    let directory = store.node_vm_directory();
    std::fs::create_dir_all(&directory).unwrap();
    let (temporary, _) = scoped_paths(&directory, &name(&run(4)).unwrap()).unwrap();
    std::fs::write(&temporary, b"{\"version\":").unwrap();
    let unrowed = lock_path(&directory, &run(5)).unwrap();
    std::fs::write(&unrowed, b"").unwrap();
    let (mut holder, said) = owner_ready("locking owner", dir.path());
    let held = lock_path(&directory, &run(6)).unwrap();
    assert_eq!(store.settle_node_vms().unwrap().rows, []);
    assert!(
        !temporary.exists() && !unrowed.exists(),
        "a crash's strays left"
    );
    assert!(held.exists(), "a live holder's lock file removed");
    drop(holder.stdin.take());
    assert!(holder.wait().unwrap().success());
    drop(said);
    assert_eq!(store.settle_node_vms().unwrap().rows, []);
    assert!(!held.exists(), "an ended holder's lock file left");
}

/// The review's side finding on node VM rows (batch 1114d's reads2; AC-T7.4.1): a run's first row is counted against the
/// rows' bound without reading any row, so writing N rows costs no row read, never N². Do write 100 node VM rows, each a
/// new run's, Ended. Expect no row file read by those writes.
#[test]
fn reads2_a_runs_first_row_is_counted_without_reading_the_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    let before = store.reads();
    for i in 0..100_u64 {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&i.to_le_bytes());
        store
            .put_node_vm(&NodeVmRecord {
                run: NodeRun::from_bytes(bytes),
                ..record(0, NodeVmPhase::Ended)
            })
            .unwrap();
    }
    let read = store.reads() - before;
    assert_eq!(read, 0, "writing 100 rows read {read} row files");
}
