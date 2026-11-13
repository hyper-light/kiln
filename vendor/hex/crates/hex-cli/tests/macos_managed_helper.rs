//! AC-T7.4.1 on macOS: the persistent managed-VM owner (`hex native-managed-helper`, the same binary)
//! under the real Recorded/Store transactions. The controller is a fixture script, never a VM: it holds
//! a descendant in its process group and ends when its lifeline ends, as the Shards VMM's managed
//! lifeline does. Each test waits on the fact it needs, within PATIENCE, and checks what is left by
//! pid and birth: the controller, its descendant and the helper itself.
#![cfg(target_os = "macos")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use hex_api::{
    Allocation, BoxId, BoxSpec, CapacityResources, ClassSpec, Network, OciSpec, Program, Rootfs,
    RuntimeCost,
};
use hex_core::record::{BoxRecord, Phase};
use hex_node::recorded::Recorded;
use hex_runtime::devices::{self, Books, ManagedLaunch};
use hex_runtime::macos::{CpuControl, MacRuntime, ManagedHelper};

/// Longest a test waits for a fact it needs: a bound, so a helper that never makes it true fails
/// the test instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(60);

/// The controller: argv as the SDK prepares it, plus the helper's `--host-lifeline FD`. Its
/// descendant is adversarial: it ignores SIGHUP and never reads the lifeline. The controller records
/// the descendant's pid under `SHARDS_HOME` (the only environment a launch carries), then reads its
/// lifeline until the helper's end closes it.
const CONTROLLER: &str = r#"#!/bin/sh
lifeline=""
while [ $# -gt 0 ]; do
  if [ "$1" = "--host-lifeline" ]; then lifeline="$2"; shift; fi
  shift
done
/bin/sh -c 'trap "" HUP; exec /bin/sleep 100000' &
echo $! > "$SHARDS_HOME/descendant.$$"
eval "/bin/cat <&$lifeline" > /dev/null
"#;

fn cost() -> RuntimeCost {
    let fixed = CapacityResources {
        cpu_milli: 1000,
        mem_bytes: shards_api::min_memory_mib(std::env::consts::ARCH) << 20,
        disk_bytes: 0,
    };
    RuntimeCost {
        id: [0x5a; 32],
        floor: fixed,
        ceiling: fixed,
        quantum: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        overhead: CapacityResources::default(),
        guest_overhead: CapacityResources::default(),
    }
}

/// One node: its Store, its macOS runtime with the managed helper configured in a private folder,
/// and the fixture controller.
struct Lab {
    dir: tempfile::TempDir,
    /// Holds the one authority `books` names weakly.
    _node: Arc<Recorded<MacRuntime>>,
    books: Weak<dyn Books>,
    /// The read ends of each launch's output: held, so the controller's writes are never SIGPIPE.
    output: std::sync::Mutex<Vec<std::io::PipeReader>>,
}

impl Lab {
    /// `rpc` bounds each helper request (the node's configured control patience).
    fn new(rpc: Duration) -> Self {
        let dir = tempfile::Builder::new().prefix("mh-").tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["helper", "cache", "home", "state"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
            std::fs::set_permissions(
                dir.path().join(name),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let controller = dir.path().join("controller");
        std::fs::write(&controller, CONTROLLER).unwrap();
        std::fs::set_permissions(&controller, std::fs::Permissions::from_mode(0o700)).unwrap();
        let model = hex_runtime::macos::machine_model().unwrap();
        let rates = hex_core::watch::seed(&model);
        let helper = dir.path().join("helper");
        let runtime = MacRuntime::new(
            rates.bytes,
            rates.processes,
            hex_core::watch::lateness_seed(&model),
        )
        .unwrap()
        // A fixture response, never a measured profile: no enforcement conclusion is drawn here.
        .with_cpu_control(CpuControl {
            cost_id: cost().id,
            response: Duration::from_millis(10),
        })
        .unwrap()
        .with_managed_helper(
            ManagedHelper::new(
                PathBuf::from(env!("CARGO_BIN_EXE_hex")),
                helper.join("helper.sock"),
                helper.join("helper.receipt"),
                rpc,
            )
            .unwrap(),
        )
        .unwrap();
        let node = Arc::new(Recorded::open(runtime, &dir.path().join("state")).unwrap());
        node.bind_native_books().unwrap();
        let authority: Arc<dyn Books> = node.clone();
        let books = Arc::downgrade(&authority);
        Self {
            dir,
            _node: node,
            books,
            output: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn record(&self, id: &BoxId) -> BoxRecord {
        let allocation: Allocation = cost().allocate(CapacityResources::default()).unwrap();
        let spec = BoxSpec {
            id: id.clone(),
            class: ClassSpec::Shards(Box::new(OciSpec {
                rootfs: Rootfs::Dir(self.path("rootfs")),
                init: Program::of(&["/bin/true"]),
                mounts: Vec::new(),
                hostname: None,
                network: Network::Private,
                hosts: Vec::new(),
            })),
            bounds: hex_api::Bounds::default(),
            owner: None,
            devices: Vec::new(),
            allocation: Some(allocation),
            route: None,
            band: 0,
        };
        spec.check().unwrap();
        BoxRecord::new(spec, Phase::Creating, Vec::new())
    }

    fn plan(&self) -> shards_api::PreparedBoot {
        let guest = shards_api::GuestSpec {
            kernel: self.path("kernel-not-opened"),
            init: shards_api::Init::Initrd(self.path("initrd-not-opened")),
            cmdline: String::new(),
            vcpus: 1,
            memory_mib: shards_api::min_memory_mib(std::env::consts::ARCH),
            gpu: false,
            gpu_devices: Vec::new(),
            disks: Vec::new(),
            shares: Vec::new(),
            ports: Vec::new(),
            binary: None,
        };
        shards_api::prepare_boot(&guest, &self.path("controller"), &self.path("v.sock"), 0)
            .unwrap()
            .with_runtime_paths(
                &shards_api::RuntimePaths::new(self.path("cache"), self.path("home")).unwrap(),
            )
            .unwrap()
    }

    /// Creating published, then the managed launch, in the canonical transaction.
    fn launch(&self, id: &BoxId) -> Result<shards_api::ManagedReceipt, hex_api::Refusal> {
        let record = self.record(id);
        let plan = self.plan();
        let (out, stdout) = std::io::pipe().unwrap();
        let (err, stderr) = std::io::pipe().unwrap();
        self.output.lock().unwrap().extend([out, err]);
        devices::with_transaction(&self.books, id, &[], |tx| {
            tx.put(&record)?;
            tx.launch_managed(
                &ManagedLaunch::Shards(&plan),
                shards_api::LaunchIo {
                    stdout,
                    stderr,
                    gpu_report: None,
                },
                PATIENCE,
            )
        })
    }

    /// The helper's pid, from its private startup receipt: `HEXH`, version, then its pid.
    fn helper_pid(&self) -> i32 {
        let receipt = std::fs::read(self.path("helper/helper.receipt")).unwrap();
        i32::try_from(u32::from_le_bytes(receipt[6..10].try_into().unwrap())).unwrap()
    }

    /// The controller's descendant, as the controller recorded it.
    fn descendant(&self, controller: u32) -> i32 {
        let path = self.path(&format!("home/descendant.{controller}"));
        wait_for("the controller to record its descendant", || {
            std::fs::read_to_string(&path).is_ok_and(|text| text.ends_with('\n'))
        });
        std::fs::read_to_string(&path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
}

/// The canonical deletion: Deleting first, then the helper's stop, the Released end, finalize (which
/// returns only once the last scope's helper has ended and been reaped) and the row's removal.
fn delete(lab: &Lab, id: &BoxId, receipt: &shards_api::ManagedReceipt) {
    devices::with_transaction(&lab.books, id, &[], |tx| {
        let mut row = tx.read(id)?.unwrap();
        row.phase = Phase::Deleting;
        tx.put(&row)
    })
    .unwrap();
    receipt.stop().unwrap();
    let end = receipt.wait_for(PATIENCE).unwrap().expect("a terminal end");
    assert_eq!(end.scope, shards_api::TerminalScope::Released, "{end:?}");
    let identity = receipt.identity().ok().cloned();
    devices::with_transaction(&lab.books, id, &[], |tx| {
        tx.finalize_managed(identity.as_ref())?;
        tx.remove(id)
    })
    .unwrap();
}

/// Signals our own child `pid` only while it is still the process born at `started`.
fn signal(pid: i32, started: u64, signal: rustix::process::Signal) {
    let birth = hex_runtime::macos::sys::rusage(pid)
        .unwrap()
        .unwrap()
        .started;
    assert_eq!(
        birth, started,
        "pid {pid} is no longer the process the test saw"
    );
    rustix::process::kill_process(rustix::process::Pid::from_raw(pid).unwrap(), signal).unwrap();
}

fn birth(pid: i32) -> u64 {
    hex_runtime::macos::sys::rusage(pid)
        .unwrap()
        .unwrap()
        .started
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

/// AC-T7.4.1 (completion): a managed scope launched through the helper runs; deleting it through the
/// canonical transaction stops it, waits for its Released end and finalizes it, and finalize returns
/// only once the helper itself has ended and been reaped. Afterwards nothing of the scope is left: the
/// controller, its descendant and the helper are gone, and the helper's endpoint with them.
#[test]
fn a_completed_scope_leaves_no_descendant_and_closes_its_helper() {
    let lab = Lab::new(PATIENCE);
    let id = BoxId::new("completed").unwrap();
    let receipt = lab.launch(&id).expect("managed launch through the helper");
    let identity = receipt.identity().unwrap().clone();
    let controller = i32::try_from(identity.pid).unwrap();
    let helper = lab.helper_pid();
    assert!(alive(controller), "the controller isn't running");
    assert!(alive(helper), "the helper isn't running");
    let descendant = lab.descendant(identity.pid);
    wait_for("the descendant to run", || alive(descendant));
    delete(&lab, &id, &receipt);
    drop(receipt);
    assert!(
        absent(helper),
        "finalize returned before the helper ended and was reaped"
    );
    assert!(!alive(controller), "the controller outlived its scope");
    wait_for("the descendant to be gone", || !alive(descendant));
    assert!(
        !lab.path("helper/helper.sock").exists(),
        "the helper's endpoint outlived it"
    );
}

/// AC-T7.4.1 (rejection): the helper cannot start the controller (its image isn't executable). The
/// launch is refused, typed; the scope stays charged and reachable, and the canonical cleanup
/// (Deleting, cold attach, stop, Released, finalize) ends it. No controller ever ran, and the helper is
/// closed: ended, reaped and its endpoint gone.
#[test]
fn a_refused_start_is_cleaned_up_and_closes_its_helper() {
    let lab = Lab::new(PATIENCE);
    let id = BoxId::new("refused").unwrap();
    std::fs::set_permissions(
        lab.path("controller"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(
        lab.launch(&id).is_err(),
        "an unexecutable controller launched"
    );
    let helper = lab.helper_pid();
    assert!(alive(helper), "the helper left a charged scope behind");
    devices::with_transaction(&lab.books, &id, &[], |tx| {
        let mut row = tx.read(&id)?.unwrap();
        assert!(row.init.is_none(), "a refused start published a controller");
        row.phase = Phase::Deleting;
        tx.put(&row)
    })
    .unwrap();
    let receipt = devices::with_transaction(&lab.books, &id, &[], |tx| tx.attach_managed(None))
        .expect("the retained scope is reachable for cleanup");
    delete(&lab, &id, &receipt);
    drop(receipt);
    assert!(absent(helper), "finalize returned before the helper ended");
    assert!(
        std::fs::read_dir(lab.path("home"))
            .unwrap()
            .next()
            .is_none(),
        "a controller ran"
    );
    assert!(!lab.path("helper/helper.sock").exists());
}

/// AC-T7.4.1 (timeout): with the helper stopped, a control request spends its whole bounded patience
/// and is refused, typed; it neither claims an end nor loses the scope. Once the helper runs again,
/// the canonical deletion ends the controller and its descendant and closes the helper.
#[test]
fn a_timed_out_request_keeps_the_scope_for_its_cleanup() {
    // The node's configured control patience for this lab: long enough for a launch on this host,
    // short enough that the timed-out request below ends the test well within PATIENCE.
    let lab = Lab::new(Duration::from_secs(5));
    let id = BoxId::new("timed-out").unwrap();
    let receipt = lab.launch(&id).expect("managed launch through the helper");
    let controller = i32::try_from(receipt.identity().unwrap().pid).unwrap();
    let descendant = lab.descendant(receipt.identity().unwrap().pid);
    let helper = lab.helper_pid();
    let born = birth(helper);
    signal(helper, born, rustix::process::Signal::STOP);
    let refused = receipt.terminal();
    signal(helper, born, rustix::process::Signal::CONT);
    assert!(refused.is_err(), "a stopped helper answered: {refused:?}");
    assert!(
        alive(controller) && alive(descendant),
        "the timeout ended the scope"
    );
    delete(&lab, &id, &receipt);
    drop(receipt);
    assert!(absent(helper), "finalize returned before the helper ended");
    assert!(!alive(controller));
    wait_for("the descendant to be gone", || !alive(descendant));
}

/// AC-T7.4.1 (worker death): SIGKILL of the helper closes every controller's lifeline (and orphans
/// its group, which the kernel hangs up), so the controller ends; the node never takes the helper's
/// absence for a Released end, finalize refuses, and the canonical row stays charged. What it can't
/// do rootless is prove a descendant that ignores SIGHUP and its lifeline gone: that needs the
/// scope's own UID (the privileged box-user helper's), the step this test names and doesn't take.
#[test]
fn a_killed_helper_ends_its_controllers_and_never_releases_their_scope() {
    let lab = Lab::new(PATIENCE);
    let id = BoxId::new("orphaned").unwrap();
    let receipt = lab.launch(&id).expect("managed launch through the helper");
    let identity = receipt.identity().unwrap().clone();
    let controller = i32::try_from(identity.pid).unwrap();
    let descendant = lab.descendant(identity.pid);
    let descendant_birth = hex_runtime::macos::sys::rusage(descendant)
        .unwrap()
        .map(|usage| usage.started);
    let helper = lab.helper_pid();
    signal(helper, birth(helper), rustix::process::Signal::KILL);
    wait_for("the controller to end with its lifeline", || {
        !alive(controller)
    });
    assert!(
        !matches!(
            receipt.terminal(),
            Ok(Some(end)) if end.scope == shards_api::TerminalScope::Released
        ),
        "the helper's death was taken for a Released end"
    );
    devices::with_transaction(&lab.books, &id, &[], |tx| {
        let mut row = tx.read(&id)?.unwrap();
        row.phase = Phase::Deleting;
        tx.put(&row)
    })
    .unwrap();
    assert!(
        devices::with_transaction(&lab.books, &id, &[], |tx| tx
            .finalize_managed(Some(&identity)))
        .is_err(),
        "finalize released a scope its dead owner never proved empty"
    );
    // Whether a descendant that ignores SIGHUP and its lifeline is still here is not something this
    // rootless owner can prove either way, which is why the row above stays charged. End it if so.
    if let Some(started) = descendant_birth
        && alive(descendant)
    {
        signal(descendant, started, rustix::process::Signal::KILL);
    }
    drop(receipt);
}
